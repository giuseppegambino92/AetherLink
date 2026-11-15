//! On-disk, content-addressed, BLAKE3-verified LRU chunk cache.
//!
//! Chunks are stored as `<dir>/<blake3-hex>.chunk`. The *key* used by
//! [`crate::VfsClient::read_cached`] is `(share, path, chunk index, mtime)`: the client
//! maps it to the chunk's BLAKE3 hash through the host's `ChunkHashes` list for that
//! `(share, path, mtime)`, then looks the hash up here. Identical chunks across files
//! are therefore stored once, and every chunk read from disk is re-verified against its
//! hash, so a corrupted or tampered cache file is detected and dropped.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::error::{Result, VfsError};

/// Hit/miss counters of a [`ChunkCache`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    /// Chunks dropped because their on-disk content no longer matched the hash.
    pub corrupt: u64,
    pub entries: usize,
    pub used_bytes: u64,
    pub budget_bytes: u64,
}

#[derive(Default)]
struct Lru {
    map: HashMap<[u8; 32], (u64, u64)>, // hash -> (size, tick)
    order: BTreeMap<u64, [u8; 32]>,     // tick -> hash
    used: u64,
    tick: u64,
}

impl Lru {
    fn touch(&mut self, h: &[u8; 32]) -> bool {
        let Some((_, t)) = self.map.get_mut(h) else {
            return false;
        };
        self.order.remove(t);
        self.tick += 1;
        *t = self.tick;
        self.order.insert(self.tick, *h);
        true
    }

    fn insert(&mut self, h: [u8; 32], size: u64) {
        if self.touch(&h) {
            return;
        }
        self.tick += 1;
        self.map.insert(h, (size, self.tick));
        self.order.insert(self.tick, h);
        self.used += size;
    }

    fn remove(&mut self, h: &[u8; 32]) -> bool {
        if let Some((size, t)) = self.map.remove(h) {
            self.order.remove(&t);
            self.used -= size;
            true
        } else {
            false
        }
    }

    fn pop_oldest(&mut self) -> Option<[u8; 32]> {
        let (_, h) = self.order.pop_first()?;
        let (size, _) = self.map.remove(&h)?;
        self.used -= size;
        Some(h)
    }
}

struct Inner {
    dir: PathBuf,
    budget: u64,
    lru: Mutex<Lru>,
    hits: AtomicU64,
    misses: AtomicU64,
    corrupt: AtomicU64,
    tmp_seq: AtomicU64,
}

/// Content-addressed LRU cache of `VFS_CHUNK_SIZE` chunks on disk, bounded by a byte budget.
/// Cheap to clone (shared state).
#[derive(Clone)]
pub struct ChunkCache(Arc<Inner>);

impl std::fmt::Debug for ChunkCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkCache")
            .field("dir", &self.0.dir)
            .field("budget", &self.0.budget)
            .finish()
    }
}

fn hex(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

impl ChunkCache {
    /// Open (or create) a cache in `dir` holding at most `budget_bytes` of chunk data.
    /// Existing chunk files are re-indexed (oldest-modified first in LRU order) and the
    /// budget is enforced immediately; leftover temp files are removed.
    pub fn open(dir: impl Into<PathBuf>, budget_bytes: u64) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let mut found = Vec::new();
        for ent in fs::read_dir(&dir)? {
            let ent = ent?;
            let name = ent.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(".tmp-") {
                let _ = fs::remove_file(ent.path());
                continue;
            }
            let Some(h) = name.strip_suffix(".chunk").and_then(unhex) else {
                continue;
            };
            let md = ent.metadata()?;
            if !md.is_file() {
                continue;
            }
            found.push((md.modified().ok(), h, md.len()));
        }
        found.sort_by_key(|(t, _, _)| *t);
        let mut lru = Lru::default();
        for (_, h, size) in found {
            lru.insert(h, size);
        }
        let cache = ChunkCache(Arc::new(Inner {
            dir,
            budget: budget_bytes,
            lru: Mutex::new(lru),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            corrupt: AtomicU64::new(0),
            tmp_seq: AtomicU64::new(0),
        }));
        cache.evict_to_budget();
        Ok(cache)
    }

    pub fn dir(&self) -> &Path {
        &self.0.dir
    }

    pub fn budget(&self) -> u64 {
        self.0.budget
    }

    pub fn used_bytes(&self) -> u64 {
        self.0.lru.lock().used
    }

    pub fn len(&self) -> usize {
        self.0.lru.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, hash: &[u8; 32]) -> bool {
        self.0.lru.lock().map.contains_key(hash)
    }

    pub fn stats(&self) -> CacheStats {
        let lru = self.0.lru.lock();
        CacheStats {
            hits: self.0.hits.load(Ordering::Relaxed),
            misses: self.0.misses.load(Ordering::Relaxed),
            corrupt: self.0.corrupt.load(Ordering::Relaxed),
            entries: lru.map.len(),
            used_bytes: lru.used,
            budget_bytes: self.0.budget,
        }
    }

    fn path_of(&self, hash: &[u8; 32]) -> PathBuf {
        self.0.dir.join(format!("{}.chunk", hex(hash)))
    }

    /// Fetch a chunk by hash. The content is re-hashed; a mismatching file is deleted
    /// and reported as a miss. Blocking file I/O.
    pub fn get(&self, hash: &[u8; 32]) -> Option<Vec<u8>> {
        if !self.0.lru.lock().touch(hash) {
            self.0.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        match fs::read(self.path_of(hash)) {
            Ok(data) if blake3::hash(&data).as_bytes() == hash => {
                self.0.hits.fetch_add(1, Ordering::Relaxed);
                Some(data)
            }
            Ok(_) => {
                self.0.corrupt.fetch_add(1, Ordering::Relaxed);
                self.0.misses.fetch_add(1, Ordering::Relaxed);
                self.remove(hash);
                None
            }
            Err(_) => {
                self.0.misses.fetch_add(1, Ordering::Relaxed);
                self.0.lru.lock().remove(hash);
                None
            }
        }
    }

    /// Store a chunk. Fails with [`VfsError::Integrity`] if `data` does not hash to `hash`.
    /// Returns `false` if the chunk is larger than the whole budget (not stored).
    pub fn insert(&self, hash: [u8; 32], data: &[u8]) -> Result<bool> {
        if blake3::hash(data).as_bytes() != &hash {
            return Err(VfsError::Integrity(
                "chunk content does not match its BLAKE3 hash".into(),
            ));
        }
        if data.len() as u64 > self.0.budget {
            return Ok(false);
        }
        if self.0.lru.lock().touch(&hash) {
            return Ok(true);
        }
        let seq = self.0.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .0
            .dir
            .join(format!(".tmp-{}-{seq}", std::process::id()));
        let res = (|| -> io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_data().ok();
            fs::rename(&tmp, self.path_of(&hash))
        })();
        if let Err(e) = res {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        self.0.lru.lock().insert(hash, data.len() as u64);
        self.evict_to_budget();
        Ok(true)
    }

    /// Drop one chunk.
    pub fn remove(&self, hash: &[u8; 32]) -> bool {
        let removed = self.0.lru.lock().remove(hash);
        let _ = fs::remove_file(self.path_of(hash));
        removed
    }

    /// Remove every cached chunk.
    pub fn clear(&self) {
        let mut lru = self.0.lru.lock();
        while let Some(h) = lru.pop_oldest() {
            let _ = fs::remove_file(self.path_of(&h));
        }
    }

    fn evict_to_budget(&self) {
        let mut lru = self.0.lru.lock();
        while lru.used > self.0.budget {
            let Some(h) = lru.pop_oldest() else { break };
            let _ = fs::remove_file(self.path_of(&h));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_budget_and_verification() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::open(dir.path(), 250).unwrap();
        let chunks: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i; 100]).collect();
        let hashes: Vec<[u8; 32]> = chunks.iter().map(|c| *blake3::hash(c).as_bytes()).collect();

        assert!(cache.insert(hashes[0], &chunks[0]).unwrap());
        assert!(cache.insert(hashes[1], &chunks[1]).unwrap());
        // Touch chunk 0 so chunk 1 is the LRU victim.
        assert_eq!(cache.get(&hashes[0]).unwrap(), chunks[0]);
        assert!(cache.insert(hashes[2], &chunks[2]).unwrap());
        assert!(cache.used_bytes() <= 250);
        assert!(cache.contains(&hashes[0]));
        assert!(!cache.contains(&hashes[1]));
        assert!(cache.contains(&hashes[2]));
        assert!(
            !dir.path()
                .join(format!("{}.chunk", hex(&hashes[1])))
                .exists()
        );

        // Wrong content is refused.
        assert!(matches!(
            cache.insert(hashes[3], &chunks[0]),
            Err(VfsError::Integrity(_))
        ));

        // On-disk corruption is detected and the entry dropped.
        fs::write(
            dir.path().join(format!("{}.chunk", hex(&hashes[2]))),
            b"garbage",
        )
        .unwrap();
        assert!(cache.get(&hashes[2]).is_none());
        assert!(!cache.contains(&hashes[2]));
        assert_eq!(cache.stats().corrupt, 1);

        // Re-open re-indexes what is on disk.
        drop(cache);
        let cache = ChunkCache::open(dir.path(), 250).unwrap();
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&hashes[0]).unwrap(), chunks[0]);

        // Oversized chunk is not stored.
        let big = vec![9u8; 300];
        assert!(!cache.insert(*blake3::hash(&big).as_bytes(), &big).unwrap());
    }
}
