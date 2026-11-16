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

#[tokio::test]
async fn overflowing_offers_and_chunks_are_rejected() {
    // Entry sizes whose sum overflows u64 (and a total that matches the wrapped sum).
    let (r, dst) = receive_raw(vec![TransferMessage::Offer {
        transfer_id: 1,
        entries: vec![file_entry("a", u64::MAX), file_entry("b", 2)],
        total_bytes: 1,
        drop_target: None,
    }])
    .await;
    assert!(matches!(r, Err(VfsError::Protocol(_))), "{r:?}");
    assert_eq!(std::fs::read_dir(dst.path()).unwrap().count(), 0);
    // A chunk with a hostile offset.
    let (r, dst) = receive_raw(vec![
        TransferMessage::Offer {
            transfer_id: 1,
            entries: vec![file_entry("f", 4)],
            total_bytes: 4,
            drop_target: None,
        },
        TransferMessage::Chunk {
            entry: 0,
            offset: u64::MAX,
            data: vec![0; 4],
        },
    ])
    .await;
    assert!(matches!(r, Err(VfsError::Protocol(_))), "{r:?}");
    assert_eq!(std::fs::read_dir(dst.path()).unwrap().count(), 0);
}

fn file_msgs(data: &[u8]) -> Vec<TransferMessage> {
    vec![
        TransferMessage::Chunk {
            entry: 0,
            offset: 0,
            data: data.to_vec(),
        },
        TransferMessage::EntryDone {
            entry: 0,
            blake3: *blake3::hash(data).as_bytes(),
        },
    ]
}

#[tokio::test]
async fn concurrent_transfers_with_the_same_id_do_not_collide() {
    let dst = tempfile::tempdir().unwrap();
    // A file named like an old-style temp file must never be truncated.
    std::fs::write(dst.path().join(".f.txt.aetherpart-7-0"), b"keep me").unwrap();
    let run = |data: &'static [u8]| {
        let d = dst.path().to_path_buf();
        async move {
            let (mut tx, mut rx) = tokio::io::duplex(1 << 16);
            let mut msgs = vec![TransferMessage::Offer {
                transfer_id: 7,
                entries: vec![file_entry("f.txt", data.len() as u64)],
                total_bytes: data.len() as u64,
                drop_target: None,
            }];
            msgs.extend(file_msgs(data));
            msgs.push(TransferMessage::Complete);
            let w = tokio::spawn(async move {
                for m in &msgs {
                    write_frame(&mut tx, m).await.unwrap();
                    tokio::task::yield_now().await;
                }
            });
            let r = receive(&mut rx, &d, ReceiveOptions::default(), |_| {}).await;
            w.await.unwrap();
            r.unwrap()
        }
    };
    let (a, b) = tokio::join!(run(b"first"), run(b"second!"));
    let mut got: Vec<Vec<u8>> = [&a, &b]
        .iter()
        .map(|s| std::fs::read(&s.files[0]).unwrap())
        .collect();
    got.sort();
    assert_eq!(got, vec![b"first".to_vec(), b"second!".to_vec()]);
    assert_ne!(a.files[0], b.files[0]);
    assert_eq!(
        std::fs::read(dst.path().join(".f.txt.aetherpart-7-0")).unwrap(),
        b"keep me"
    );
    let names: Vec<String> = std::fs::read_dir(dst.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with(".aetherpart-"))
        .collect();
    assert!(names.is_empty(), "staging left behind: {names:?}");
}

#[tokio::test]
async fn failed_transfer_leaves_the_destination_untouched() {
    let good = b"good".to_vec();
    let (r, dst) = receive_raw(vec![
        TransferMessage::Offer {
            transfer_id: 1,
            entries: vec![
                aether_protocol::TransferEntry {
                    rel_path: "d".into(),
                    size: 0,
                    is_dir: true,
                    mtime_ns: 0,
                },
                aether_protocol::TransferEntry {
                    rel_path: "d/empty_dir".into(),
                    size: 0,
                    is_dir: true,
                    mtime_ns: 0,
                },
                file_entry("d/a", 4),
                file_entry("d/b", 3),
                file_entry("top.txt", 0),
            ],
            total_bytes: 7,
            drop_target: None,
        },
        TransferMessage::Chunk {
            entry: 2,
            offset: 0,
            data: good.clone(),
        },
        TransferMessage::EntryDone {
            entry: 2,
            blake3: *blake3::hash(&good).as_bytes(),
        },
        TransferMessage::EntryDone {
            entry: 4,
            blake3: *blake3::hash(b"").as_bytes(),
        },
        TransferMessage::Chunk {
            entry: 3,
            offset: 0,
            data: b"bad".to_vec(),
        },
        TransferMessage::EntryDone {
            entry: 3,
            blake3: *blake3::hash(b"xyz").as_bytes(),
        },
        TransferMessage::Complete,
    ])
    .await;
    assert!(matches!(r, Err(VfsError::Integrity(_))), "{r:?}");
    // No directories, verified files or staging remain (all-or-nothing).
    let left: Vec<_> = std::fs::read_dir(dst.path()).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn staging_directory_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dst = tempfile::tempdir().unwrap();
    let (mut tx, mut rx) = tokio::io::duplex(1 << 16);
    write_frame(
        &mut tx,
        &TransferMessage::Offer {
            transfer_id: 1,
            entries: vec![file_entry("f", 1)],
            total_bytes: 1,
            drop_target: None,
        },
    )
    .await
    .unwrap();
    let d = dst.path().to_path_buf();
    let recv = tokio::spawn(async move {
        receive(&mut rx, &d, ReceiveOptions::default(), |_| {}).await
    });
    // Wait for the staging directory to appear, then inspect it mid-transfer.
    let staging = loop {
        if let Some(e) = std::fs::read_dir(dst.path())
            .unwrap()
            .map(|e| e.unwrap())
            .find(|e| e.file_name().to_str().unwrap().starts_with(".aetherpart-"))
        {
            break e.path();
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    let name = staging.file_name().unwrap().to_str().unwrap().to_string();
    assert_eq!(name.len(), ".aetherpart-".len() + 32, "{name}");
    let mode = std::fs::metadata(&staging).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    drop(tx); // abort: stream ends before Complete
    assert!(recv.await.unwrap().is_err());
    assert_eq!(std::fs::read_dir(dst.path()).unwrap().count(), 0);
}
