use aether_protocol::{TelemetrySample, TimelineEventKind, TimelineMark};
use aether_timetravel::{Blackbox, BlackboxConfig, ENTRY_OVERHEAD, TimeTravelError};

const FRAME_US: u64 = 16_667; // 60 fps

/// Deterministic payload that encodes its own pts, so decrypted data can be checked.
fn payload(pts: u64, keyframe: bool, len: usize) -> Vec<u8> {
    let mut v = vec![if keyframe { 0xAA } else { 0x55 }; len.max(9)];
    v[..8].copy_from_slice(&pts.to_le_bytes());
    v
}

fn pts_of(data: &[u8]) -> u64 {
    u64::from_le_bytes(data[..8].try_into().unwrap())
}

fn cfg(window_s: u64, max_bytes: usize) -> BlackboxConfig {
    BlackboxConfig {
        window_us: window_s * 1_000_000,
        max_bytes,
        spike: None,
        ..Default::default()
    }
}

/// Push `n` frames at 60 fps with a keyframe every `gop` frames.
fn feed(bb: &mut Blackbox, start_frame: u64, n: u64, gop: u64, kf_len: usize, p_len: usize) {
    for i in start_frame..start_frame + n {
        let kf = i % gop == 0;
        let pts = i * FRAME_US;
        bb.push_frame(pts, kf, &payload(pts, kf, if kf { kf_len } else { p_len }));
    }
}

#[test]
fn seek_returns_keyframe_and_following_frames() {
    let mut bb = Blackbox::new(cfg(180, 64 << 20));
    feed(&mut bb, 0, 300, 60, 1000, 100); // keyframes at frames 0, 60, 120, 180, 240

    let target = 130 * FRAME_US + 5; // between frames 130 and 131
    let plan = bb.seek(target).unwrap();
    assert_eq!(plan.keyframe_pts_us, 120 * FRAME_US);
    assert_eq!(plan.target_pts_us, target);
    assert_eq!(plan.frames.len(), 11); // frames 120..=130
    assert!(plan.frames[0].keyframe);
    assert!(plan.frames[1..].iter().all(|f| !f.keyframe));
    for (i, f) in plan.frames.iter().enumerate() {
        assert_eq!(f.pts_us, (120 + i as u64) * FRAME_US);
        assert_eq!(
            pts_of(&f.data),
            f.pts_us,
            "payload decrypts to the original"
        );
    }
    // Exactly on a keyframe → just that frame.
    let plan = bb.seek(180 * FRAME_US).unwrap();
    assert_eq!(plan.frames.len(), 1);
    // Past the end clamps to the newest frame.
    let plan = bb.seek(u64::MAX).unwrap();
    assert_eq!(plan.target_pts_us, 299 * FRAME_US);
    assert_eq!(plan.frames.len(), 60);

    let idx = bb.index();
    assert_eq!(idx.start_us, 0);
    assert_eq!(idx.end_us, 299 * FRAME_US);
    assert_eq!(
        idx.keyframes,
        (0..5).map(|k| k * 60 * FRAME_US).collect::<Vec<_>>()
    );

    let range = bb.frames_in_range(10 * FRAME_US, 12 * FRAME_US).unwrap();
    assert_eq!(
        range.iter().map(|f| f.pts_us).collect::<Vec<_>>(),
        vec![10 * FRAME_US, 11 * FRAME_US, 12 * FRAME_US]
    );
}

#[test]
fn empty_and_out_of_range() {
    let mut bb = Blackbox::new(cfg(180, 1 << 20));
    assert_eq!(bb.seek(0), Err(TimeTravelError::Empty));
    // Frames before the first keyframe cannot be decoded and are dropped.
    bb.push_frame(1000, false, b"orphan");
    assert!(bb.needs_keyframe());
    assert_eq!(bb.frame_count(), 0);
    assert_eq!(bb.dropped_frames(), 1);
    bb.push_frame(5000, true, b"key");
    assert!(!bb.needs_keyframe());
    assert!(matches!(
        bb.seek(4999),
        Err(TimeTravelError::OutOfRange {
            requested: 4999,
            start: 5000,
            ..
        })
    ));
    // Timestamp going backwards = stream restart.
    bb.push_frame(100, false, b"restart");
    assert_eq!(bb.frame_count(), 0);
    assert!(bb.needs_keyframe());
}

#[test]
fn window_eviction_keeps_gop_boundary() {
    let window_s = 10;
    let mut bb = Blackbox::new(cfg(window_s, usize::MAX / 4));
    let gop = 90; // 1.5 s GOPs, not aligned with the window
    for i in 0..(60 * 30) {
        let kf = i % gop == 0;
        let pts = i * FRAME_US;
        bb.push_frame(pts, kf, &payload(pts, kf, 64));
        let start = bb.start_us().unwrap();
        let end = bb.end_us().unwrap();
        // Oldest retained frame is always a keyframe…
        assert_eq!(bb.index().keyframes[0], start);
        let first = bb.frames_in_range(start, start).unwrap();
        assert!(first[0].keyframe);
        // …the window is covered…
        assert!(
            start <= end.saturating_sub(window_s * 1_000_000),
            "window not covered at frame {i}"
        );
        // …and at most one extra GOP is retained.
        assert!(
            end - start < window_s * 1_000_000 + gop * FRAME_US,
            "too much retained at frame {i}"
        );
    }
    // Everything retained is seekable.
    let s = bb.start_us().unwrap();
    for t in [s, s + 1, s + 3_000_000, bb.end_us().unwrap()] {
        let plan = bb.seek(t).unwrap();
        assert!(plan.frames[0].keyframe);
        assert!(plan.frames.last().unwrap().pts_us <= t);
    }
}

#[test]
fn byte_budget_eviction() {
    let budget = 200_000;
    let mut bb = Blackbox::new(cfg(180, budget));
    // ~ (10_000 + 59 * 1_000) bytes + overhead per 1 s GOP → only ~2 GOPs fit.
    for i in 0..(60 * 20u64) {
        let kf = i % 60 == 0;
        let pts = i * FRAME_US;
        bb.push_frame(pts, kf, &payload(pts, kf, if kf { 10_000 } else { 1_000 }));
        // The current GOP is always retained; at most one GOP worth over the budget
        // only while the front GOP is the current one.
        if bb.index().keyframes.len() >= 2 {
            assert!(
                bb.bytes_used() <= budget,
                "over budget at frame {i}: {}",
                bb.bytes_used()
            );
        }
    }
    let idx = bb.index();
    assert!(
        idx.keyframes.len() >= 2 && idx.keyframes.len() <= 3,
        "{:?}",
        idx.keyframes
    );
    assert_eq!(idx.start_us, idx.keyframes[0]);
    assert!(bb.duration_us() < 3_000_000);
    assert!(bb.seek(idx.start_us).is_ok());

    // A pathological GOP (no keyframes) larger than 2× budget is dropped entirely and the
    // recorder asks for a keyframe instead of growing without bound.
    let mut bb = Blackbox::new(cfg(180, 50_000));
    bb.push_frame(0, true, &[0; 1000]);
    for i in 1..1000u64 {
        bb.push_frame(i * FRAME_US, false, &[0; 1000]);
        assert!(bb.bytes_used() <= 2 * 50_000 + 1000 + ENTRY_OVERHEAD + 16);
    }
    assert!(bb.needs_keyframe());
}

#[test]
fn marks_payloads_and_telemetry_spikes() {
    let mut bb = Blackbox::new(BlackboxConfig {
        window_us: 60_000_000,
        max_bytes: 64 << 20,
        ..Default::default()
    });
    feed(&mut bb, 0, 600, 60, 500, 50);
    let clip = TimelineMark {
        pts_us: 100 * FRAME_US,
        kind: TimelineEventKind::Clipboard,
        label: Some("copied".into()),
    };
    bb.push_mark(clip.clone(), Some(b"secret clipboard text"));
    let alert = TimelineMark {
        pts_us: 50 * FRAME_US,
        kind: TimelineEventKind::Alert,
        label: None,
    };
    bb.push_mark(alert.clone(), None);
    assert_eq!(
        bb.marks(),
        vec![alert.clone(), clip.clone()],
        "marks sorted by time"
    );
    assert_eq!(
        bb.mark_payload(&clip).unwrap().unwrap(),
        b"secret clipboard text"
    );
    assert_eq!(bb.mark_payload(&alert).unwrap(), None);

    for i in 0..40u64 {
        let cpu = if i == 30 { 97.0 } else { 20.0 + (i % 4) as f32 };
        bb.push_telemetry(TelemetrySample {
            pts_us: i * 250_000,
            host_cpu_percent: cpu,
            host_mem_percent: 50.0,
            host_net_tx_kbps: 2_000,
            rtt_ms: 15.0,
            ..Default::default()
        });
    }
    let spikes: Vec<_> = bb
        .index()
        .marks
        .into_iter()
        .filter(|m| m.kind == TimelineEventKind::ResourceSpike)
        .collect();
    assert_eq!(spikes.len(), 1, "{spikes:?}");
    assert_eq!(spikes[0].pts_us, 30 * 250_000);
    assert_eq!(spikes[0].label.as_deref(), Some("CPU 97%"));
    assert_eq!(bb.telemetry().count(), 40);

    // Marks older than the oldest frame are evicted together with their GOP.
    feed(&mut bb, 600, 60 * 70, 60, 500, 50);
    assert!(
        bb.marks()
            .iter()
            .all(|m| m.pts_us >= bb.start_us().unwrap())
    );
    assert!(bb.mark_payload(&clip).unwrap().is_none());
}

#[test]
fn export_import_bundle() {
    let mut bb = Blackbox::new(cfg(180, 64 << 20));
    feed(&mut bb, 0, 200, 60, 300, 30);
    let mark = TimelineMark {
        pts_us: 10 * FRAME_US,
        kind: TimelineEventKind::Terminal,
        label: Some("ls".into()),
    };
    bb.push_mark(mark.clone(), Some(b"$ ls -la"));
    let key = [7u8; 32];
    let bundle = bb.export_bundle(&key).unwrap();
    assert!(
        !bundle.windows(8).any(|w| w == b"$ ls -la"),
        "bundle is encrypted"
    );

    let restored = Blackbox::import_bundle(&bundle, &key, cfg(180, 64 << 20)).unwrap();
    assert_eq!(restored.index(), bb.index());
    assert_eq!(
        restored.seek(150 * FRAME_US).unwrap(),
        bb.seek(150 * FRAME_US).unwrap()
    );
    assert_eq!(restored.mark_payload(&mark).unwrap().unwrap(), b"$ ls -la");

    assert_eq!(
        Blackbox::import_bundle(&bundle, &[8u8; 32], cfg(180, 1 << 20)).unwrap_err(),
        TimeTravelError::Tampered
    );
    let mut bad = bundle.clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    assert_eq!(
        Blackbox::import_bundle(&bad, &key, cfg(180, 1 << 20)).unwrap_err(),
        TimeTravelError::Tampered
    );
    assert!(matches!(
        Blackbox::import_bundle(b"nope", &key, cfg(180, 1 << 20)),
        Err(TimeTravelError::Bundle(_))
    ));
}

/// 60 fps for longer than the 180 s window with realistic-ish frame sizes; memory must
/// stay bounded by the byte budget and the time window throughout.
#[test]
fn load_60fps_180s_bounded_memory() {
    let budget = 16 * 1024 * 1024;
    let mut bb = Blackbox::new(BlackboxConfig {
        max_bytes: budget,
        spike: None,
        ..Default::default()
    });
    // Sizes are scaled down (it is the *bounds* that are under test, and debug builds
    // monomorphize the AEAD at opt-level 0); the budget is scaled to match.
    let kf = vec![0x11u8; 8_000];
    let p = vec![0x22u8; 800];
    let gop = 120; // keyframe every 2 s
    let total_frames = 60 * 200; // 200 s of video
    let mut max_bytes = 0;
    let mut max_frames = 0;
    for i in 0..total_frames {
        let is_kf = i % gop == 0;
        bb.push_frame(i * FRAME_US, is_kf, if is_kf { &kf } else { &p });
        max_bytes = max_bytes.max(bb.bytes_used());
        max_frames = max_frames.max(bb.frame_count());
        if i % 60 == 0 {
            bb.push_telemetry(TelemetrySample {
                pts_us: i * FRAME_US,
                host_cpu_percent: 10.0,
                ..Default::default()
            });
        }
    }
    // Budget never exceeded (GOP ≪ budget here).
    assert!(max_bytes <= budget, "{max_bytes} > {budget}");
    // Window bound: ≤ 180 s + one GOP of frames.
    assert!(max_frames as u64 <= 180 * 60 + gop, "{max_frames}");
    assert!(bb.duration_us() <= 180_000_000 + gop * FRAME_US);
    assert!(bb.duration_us() >= 180_000_000 - gop * FRAME_US);
    // ≈ 10.5 MB for 180 s: under the budget, so the window is the binding limit.
    let plan = bb.seek(bb.end_us().unwrap()).unwrap();
    assert!(plan.frames[0].keyframe && plan.frames.len() <= gop as usize);
    assert!(bb.telemetry().count() <= 184);

    // Now with a tight budget the byte limit binds instead.
    let budget = 1024 * 1024;
    let mut bb = Blackbox::new(BlackboxConfig {
        max_bytes: budget,
        spike: None,
        ..Default::default()
    });
    for i in 0..total_frames {
        let is_kf = i % gop == 0;
        bb.push_frame(i * FRAME_US, is_kf, if is_kf { &kf } else { &p });
        if bb.index().keyframes.len() >= 2 {
            assert!(bb.bytes_used() <= budget);
        }
    }
    assert!(bb.duration_us() < 180_000_000);
    assert_eq!(bb.index().keyframes[0], bb.start_us().unwrap());
}
