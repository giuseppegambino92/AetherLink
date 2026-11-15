//! File-transfer round trips over an in-memory duplex stream.

use std::path::PathBuf;

use aether_protocol::TransferMessage;
use aether_protocol::framing::write_frame;
use aether_vfs::VfsError;
use aether_vfs::transfer::{ReceiveOptions, SendOptions, TransferProgress, receive, send_paths};

fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect()
}

fn leftovers(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains(".aetherpart-")
        {
            out.push(p.clone());
        }
        if p.is_dir() {
            out.extend(leftovers(&p));
        }
    }
    out
}

#[tokio::test]
async fn transfer_roundtrip_with_collisions() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let s = src.path();
    std::fs::write(s.join("a.txt"), b"alpha").unwrap();
    std::fs::create_dir_all(s.join("dir/sub")).unwrap();
    let big = pattern(2 * 1024 * 1024 + 777, 3);
    std::fs::write(s.join("dir/b.bin"), &big).unwrap();
    std::fs::write(s.join("dir/sub/c.txt"), b"gamma").unwrap();
    std::fs::write(s.join("dir/sub/empty"), b"").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/passwd", s.join("dir/link")).unwrap();
    // Pre-existing names in the destination force collision renaming.
    std::fs::write(dst.path().join("a.txt"), b"existing").unwrap();
    std::fs::create_dir(dst.path().join("dir")).unwrap();

    let (mut tx, mut rx) = tokio::io::duplex(64 * 1024);
    let paths = vec![s.join("a.txt"), s.join("dir")];
    let sender = tokio::spawn(async move {
        let mut last = None;
        let n = send_paths(
            &paths,
            &mut tx,
            SendOptions {
                transfer_id: 42,
                ..Default::default()
            },
            |p| last = Some(p.clone()),
        )
        .await
        .unwrap();
        (n, last.unwrap())
    });
    let mut events: Vec<TransferProgress> = Vec::new();
    let summary = receive(&mut rx, dst.path(), ReceiveOptions::default(), |p| {
        events.push(p.clone())
    })
    .await
    .unwrap();
    let (sent, last_send) = sender.await.unwrap();

    let total = 5 + big.len() as u64 + 5;
    assert_eq!(sent, total);
    assert_eq!(last_send.bytes_done, total);
    assert_eq!(summary.transfer_id, 42);
    assert_eq!(summary.bytes, total);
    let last = events.last().unwrap();
    assert_eq!(
        (
            last.bytes_done,
            last.total_bytes,
            last.entries_done,
            last.entries_total
        ),
        (total, total, 4, 4)
    );

    let d = dst.path().canonicalize().unwrap();
    assert_eq!(summary.roots, vec![d.join("a (1).txt"), d.join("dir (1)")]);
    assert_eq!(std::fs::read(d.join("a.txt")).unwrap(), b"existing");
    assert_eq!(std::fs::read(d.join("a (1).txt")).unwrap(), b"alpha");
    assert_eq!(std::fs::read(d.join("dir (1)/b.bin")).unwrap(), big);
    assert_eq!(
        std::fs::read(d.join("dir (1)/sub/c.txt")).unwrap(),
        b"gamma"
    );
    assert_eq!(std::fs::read(d.join("dir (1)/sub/empty")).unwrap(), b"");
    assert!(
        !d.join("dir (1)/link").exists(),
        "symlinks are not transferred"
    );
    assert_eq!(summary.files.len(), 4);
    assert!(leftovers(&d).is_empty());
}

async fn receive_raw(
    msgs: Vec<TransferMessage>,
) -> (
    Result<aether_vfs::transfer::ReceiveSummary, VfsError>,
    tempfile::TempDir,
) {
    let dst = tempfile::tempdir().unwrap();
    let (mut tx, mut rx) = tokio::io::duplex(1 << 20);
    for m in &msgs {
        write_frame(&mut tx, m).await.unwrap();
    }
    drop(tx);
    let r = receive(&mut rx, dst.path(), ReceiveOptions::default(), |_| {}).await;
    (r, dst)
}

fn file_entry(rel: &str, size: u64) -> aether_protocol::TransferEntry {
    aether_protocol::TransferEntry {
        rel_path: rel.into(),
        size,
        is_dir: false,
        mtime_ns: 0,
    }
}

#[tokio::test]
async fn corrupted_entry_is_rejected_and_cleaned_up() {
    let data = b"payload".to_vec();
    let (r, dst) = receive_raw(vec![
        TransferMessage::Offer {
            transfer_id: 1,
            entries: vec![file_entry("f.txt", 7)],
            total_bytes: 7,
            drop_target: None,
        },
        TransferMessage::Chunk {
            entry: 0,
            offset: 0,
            data: data.clone(),
        },
        TransferMessage::EntryDone {
            entry: 0,
            blake3: *blake3::hash(b"different").as_bytes(),
        },
        TransferMessage::Complete,
    ])
    .await;
    assert!(matches!(r, Err(VfsError::Integrity(_))), "{r:?}");
    assert_eq!(
        std::fs::read_dir(dst.path()).unwrap().count(),
        0,
        "no partial or temp files left"
    );
}

#[tokio::test]
async fn hostile_offers_are_rejected() {
    for rel in [
        "../evil.txt",
        "/etc/evil",
        "a/../../evil",
        "C:/evil",
        "ok/\0bad",
        "",
    ] {
        let (r, dst) = receive_raw(vec![TransferMessage::Offer {
            transfer_id: 1,
            entries: vec![file_entry(rel, 1)],
            total_bytes: 1,
            drop_target: None,
        }])
        .await;
        assert!(matches!(r, Err(VfsError::InvalidPath(_))), "{rel:?}: {r:?}");
        assert_eq!(std::fs::read_dir(dst.path()).unwrap().count(), 0);
    }
    // Data beyond the offered size.
    let (r, dst) = receive_raw(vec![
        TransferMessage::Offer {
            transfer_id: 1,
            entries: vec![file_entry("f", 2)],
            total_bytes: 2,
            drop_target: None,
        },
        TransferMessage::Chunk {
            entry: 0,
            offset: 0,
            data: vec![0; 3],
        },
    ])
    .await;
    assert!(matches!(r, Err(VfsError::Protocol(_))), "{r:?}");
    assert_eq!(std::fs::read_dir(dst.path()).unwrap().count(), 0);
    // Sender cancellation.
    let (r, dst) = receive_raw(vec![
        TransferMessage::Offer {
            transfer_id: 1,
            entries: vec![file_entry("f", 4)],
            total_bytes: 4,
            drop_target: None,
        },
        TransferMessage::Chunk {
            entry: 0,
            offset: 0,
            data: vec![0; 2],
        },
        TransferMessage::Cancel {
            reason: "user abort".into(),
        },
    ])
    .await;
    assert!(
        matches!(r, Err(VfsError::Cancelled(ref m)) if m == "user abort"),
        "{r:?}"
    );
    assert_eq!(std::fs::read_dir(dst.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn sender_cancel_sends_cancel_message() {
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("f.bin"), pattern(3 * 1024 * 1024, 9)).unwrap();
    let dst = tempfile::tempdir().unwrap();
    let (mut tx, mut rx) = tokio::io::duplex(1 << 16);
    let opts = SendOptions {
        transfer_id: 5,
        ..Default::default()
    };
    let cancel = opts.cancel.clone();
    let paths = vec![src.path().join("f.bin")];
    let sender = tokio::spawn(async move {
        send_paths(&paths, &mut tx, opts, |p| {
            if p.bytes_done > 0 {
                cancel.cancel();
            }
        })
        .await
    });
    let r = receive(&mut rx, dst.path(), ReceiveOptions::default(), |_| {}).await;
    assert!(matches!(r, Err(VfsError::Cancelled(_))), "{r:?}");
    assert!(matches!(sender.await.unwrap(), Err(VfsError::Cancelled(_))));
    assert!(leftovers(dst.path()).is_empty());
}
