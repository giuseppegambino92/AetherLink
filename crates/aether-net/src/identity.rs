//! Ed25519 device identity and the trusted-peer store.

use crate::{NetError, Result};
use aether_protocol::{DeviceId, KeyFingerprint};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// File name of the identity fallback store inside the `dir` given to [`Identity::load_or_create`].
pub const IDENTITY_FILE: &str = "identity.key";
/// Keychain account name under which the secret key is stored.
#[cfg_attr(not(any(target_os = "macos", target_os = "ios", target_os = "windows")), allow(dead_code))]
const KEYCHAIN_ACCOUNT: &str = "device-identity-ed25519";
/// Set to any value to bypass the OS keychain (headless servers, CI, tests).
pub const NO_KEYCHAIN_ENV: &str = "AETHERLINK_NO_KEYCHAIN";

/// An Ed25519 keypair identifying this device.
#[derive(Clone)]
pub struct Identity {
    signing: SigningKey,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("device_id", &self.device_id().to_string())
            .field("fingerprint", &self.fingerprint().to_string())
            .finish_non_exhaustive()
    }
}

impl Identity {
    pub fn generate() -> Self {
        Self { signing: SigningKey::generate(&mut rand::rngs::OsRng) }
    }

    pub fn from_secret_bytes(b: [u8; 32]) -> Self {
        Self { signing: SigningKey::from_bytes(&b) }
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Stable 9-digit ID derived from the public key.
    pub fn device_id(&self) -> DeviceId {
        DeviceId::from_public_key(&self.public_key())
    }

    pub fn fingerprint(&self) -> KeyFingerprint {
        KeyFingerprint::of(&self.public_key())
    }

    /// Ed25519 signature over `msg`.
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }

    /// Verify an Ed25519 signature (strict verification: rejects small-order / malleable inputs).
    pub fn verify(public_key: &[u8; 32], msg: &[u8], signature: &[u8]) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(public_key) else { return false };
        let Ok(sig) = ed25519_dalek::Signature::from_slice(signature) else { return false };
        vk.verify_strict(msg, &sig).is_ok() && vk.verify(msg, &sig).is_ok()
    }

    /// Load the identity from the OS keychain (`keyring`; service = `service`), falling back to
    /// a `0600` file `dir/identity.key`. Creates and persists a new identity if none exists.
    ///
    /// On platforms without a supported keychain backend (or when the `AETHERLINK_NO_KEYCHAIN`
    /// environment variable is set) only the file store is used.
    pub fn load_or_create(service: &str, dir: &Path) -> Result<Self> {
        let use_keychain = std::env::var_os(NO_KEYCHAIN_ENV).is_none() && keychain::AVAILABLE;
        if use_keychain {
            match keychain::load(service) {
                Ok(Some(id)) => return Ok(id),
                Ok(None) => {}
                Err(e) => tracing::warn!("keychain unavailable, using file store: {e}"),
            }
        }
        let file = dir.join(IDENTITY_FILE);
        if let Some(id) = load_file(&file)? {
            return Ok(id);
        }
        let id = Self::generate();
        if use_keychain {
            match keychain::store(service, &id) {
                Ok(()) => {
                    // Read back to make sure the keychain really persisted it.
                    if matches!(keychain::load(service), Ok(Some(ref back)) if back.public_key() == id.public_key()) {
                        return Ok(id);
                    }
                    tracing::warn!("keychain did not persist the identity; using file store");
                }
                Err(e) => tracing::warn!("cannot store identity in keychain ({e}); using file store"),
            }
        }
        store_file(&file, &id)?;
        Ok(id)
    }

    /// File-only variant of [`Identity::load_or_create`] (never touches the keychain).
    pub fn load_or_create_file(dir: &Path) -> Result<Self> {
        let file = dir.join(IDENTITY_FILE);
        if let Some(id) = load_file(&file)? {
            return Ok(id);
        }
        let id = Self::generate();
        store_file(&file, &id)?;
        Ok(id)
    }
}

fn decode_secret_hex(s: &str) -> Result<Identity> {
    let s = s.trim();
    let mut bytes = Zeroizing::new([0u8; 32]);
    hex::decode_to_slice(s, &mut bytes[..]).map_err(|e| NetError::Store(format!("corrupt identity: {e}")))?;
    Ok(Identity::from_secret_bytes(*bytes))
}

fn load_file(path: &Path) -> Result<Option<Identity>> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let s = Zeroizing::new(s);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(path)
                    && meta.permissions().mode() & 0o077 != 0
                {
                    tracing::warn!("{} is accessible by other users; tightening to 0600", path.display());
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
                }
            }
            decode_secret_hex(&s).map(Some)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn store_file(path: &Path, id: &Identity) -> Result<()> {
    let contents = Zeroizing::new(hex::encode(id.secret_bytes()));
    write_private_file(path, contents.as_bytes())
}

/// Atomically write `data` to `path` with owner-only permissions (0600; dir 0700 when created).
pub(crate) fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
        && !dir.exists()
    {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
    }
    let tmp: PathBuf = {
        let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(format!(".tmp{}", std::process::id()));
        path.with_file_name(name)
    };
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    {
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
mod keychain {
    use super::*;

    pub const AVAILABLE: bool = true;

    pub fn load(service: &str) -> Result<Option<Identity>> {
        let entry = keyring::Entry::new(service, KEYCHAIN_ACCOUNT).map_err(|e| NetError::Keychain(e.to_string()))?;
        match entry.get_password() {
            Ok(s) => {
                let s = Zeroizing::new(s);
                decode_secret_hex(&s).map(Some)
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(NetError::Keychain(e.to_string())),
        }
    }

    pub fn store(service: &str, id: &Identity) -> Result<()> {
        let entry = keyring::Entry::new(service, KEYCHAIN_ACCOUNT).map_err(|e| NetError::Keychain(e.to_string()))?;
        let hexed = Zeroizing::new(hex::encode(id.secret_bytes()));
        entry.set_password(&hexed).map_err(|e| NetError::Keychain(e.to_string()))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows")))]
mod keychain {
    use super::*;

    /// No keychain backend compiled in for this OS (Linux Secret Service would need libdbus);
    /// the 0600 file store is used.
    pub const AVAILABLE: bool = false;

    pub fn load(_service: &str) -> Result<Option<Identity>> {
        Err(NetError::Unsupported("OS keychain"))
    }

    pub fn store(_service: &str, _id: &Identity) -> Result<()> {
        Err(NetError::Unsupported("OS keychain"))
    }
}

// ---------------------------------------------------------------------------------------------
// Trust store
// ---------------------------------------------------------------------------------------------

/// A peer whose identity key is pinned (paired device / trusted fleet member).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    #[serde(with = "hex_key")]
    pub public_key: [u8; 32],
    pub device_id: DeviceId,
    pub name: String,
    /// Unix seconds when the peer was added.
    #[serde(default)]
    pub added_at: u64,
}

impl TrustedPeer {
    pub fn new(public_key: [u8; 32], device_id: DeviceId, name: impl Into<String>) -> Self {
        let added_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self { public_key, device_id, name: name.into(), added_at }
    }

    pub fn fingerprint(&self) -> KeyFingerprint {
        KeyFingerprint::of(&self.public_key)
    }
}

mod hex_key {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(k: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(k))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out).map_err(D::Error::custom)?;
        Ok(out)
    }
}

#[derive(Serialize, Deserialize, Default)]
struct TrustFile {
    version: u32,
    peers: Vec<TrustedPeer>,
}

/// JSON-persisted set of trusted peers keyed by public key.
#[derive(Debug)]
pub struct TrustStore {
    path: PathBuf,
    peers: BTreeMap<[u8; 32], TrustedPeer>,
}

impl TrustStore {
    /// Open (or lazily create on first write) the store at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let peers = match fs::read(path) {
            Ok(bytes) if bytes.iter().all(|b| b.is_ascii_whitespace()) => BTreeMap::new(),
            Ok(bytes) => {
                let file: TrustFile = serde_json::from_slice(&bytes)
                    .map_err(|e| NetError::Store(format!("corrupt trust store {}: {e}", path.display())))?;
                file.peers.into_iter().map(|p| (p.public_key, p)).collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path: path.to_path_buf(), peers })
    }

    pub fn is_trusted(&self, key: &[u8; 32]) -> bool {
        self.peers.contains_key(key)
    }

    pub fn get(&self, key: &[u8; 32]) -> Option<&TrustedPeer> {
        self.peers.get(key)
    }

    pub fn find_by_device_id(&self, id: DeviceId) -> Option<&TrustedPeer> {
        self.peers.values().find(|p| p.device_id == id)
    }

    /// Add or replace a peer and persist.
    pub fn add(&mut self, peer: TrustedPeer) -> Result<()> {
        self.peers.insert(peer.public_key, peer);
        self.save()
    }

    /// Remove a peer (no-op if absent) and persist.
    pub fn remove(&mut self, key: &[u8; 32]) -> Result<()> {
        if self.peers.remove(key).is_some() {
            self.save()?;
        }
        Ok(())
    }

    pub fn list(&self) -> Vec<TrustedPeer> {
        let mut v: Vec<_> = self.peers.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.device_id.cmp(&b.device_id)));
        v
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn save(&self) -> Result<()> {
        let file = TrustFile { version: 1, peers: self.list() };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| NetError::Store(e.to_string()))?;
        write_private_file(&self.path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_roundtrip_and_sign() {
        let id = Identity::generate();
        let again = Identity::from_secret_bytes(id.secret_bytes());
        assert_eq!(id.public_key(), again.public_key());
        assert_eq!(id.device_id(), DeviceId::from_public_key(&id.public_key()));
        let sig = id.sign(b"hello");
        assert!(Identity::verify(&id.public_key(), b"hello", &sig));
        assert!(!Identity::verify(&id.public_key(), b"hellO", &sig));
        assert!(!Identity::verify(&Identity::generate().public_key(), b"hello", &sig));
    }

    #[test]
    fn file_store_persists_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("nested");
        let a = Identity::load_or_create_file(&sub).unwrap();
        let b = Identity::load_or_create_file(&sub).unwrap();
        assert_eq!(a.public_key(), b.public_key());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(sub.join(IDENTITY_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn trust_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        let k1 = Identity::generate();
        let k2 = Identity::generate();
        {
            let mut ts = TrustStore::open(&path).unwrap();
            assert!(ts.list().is_empty());
            ts.add(TrustedPeer::new(k1.public_key(), k1.device_id(), "Laptop")).unwrap();
            ts.add(TrustedPeer::new(k2.public_key(), k2.device_id(), "Desktop")).unwrap();
        }
        let mut ts = TrustStore::open(&path).unwrap();
        assert!(ts.is_trusted(&k1.public_key()));
        assert_eq!(ts.list().iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Desktop", "Laptop"]);
        assert_eq!(ts.find_by_device_id(k2.device_id()).unwrap().name, "Desktop");
        ts.remove(&k1.public_key()).unwrap();
        let ts = TrustStore::open(&path).unwrap();
        assert!(!ts.is_trusted(&k1.public_key()));
        assert!(ts.is_trusted(&k2.public_key()));
    }
}
