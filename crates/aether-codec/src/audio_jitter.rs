//! Adaptive jitter buffer for real-time audio packets (pure logic, no clocks or threads).
//!
//! * Packets are reordered by `seq`; duplicates and packets whose playout slot has passed
//!   ("late") are dropped.
//! * Playout is *pulled*: the consumer (audio device clock) calls [`JitterBuffer::pop`] once per
//!   frame. A missing frame yields [`Playout::Lost`] together with the following packet when it
//!   is already buffered, so the decoder can recover it from in-band FEC (else PLC).
//! * The target delay adapts to measured network jitter: the 95th percentile of
//!   `arrival − pts` (relative to its minimum over the last ~1.3 s) plus one frame, clamped to
//!   `[min_delay_us, max_delay_us]`. The playout delay of each packet (`now − pts − min transit`)
//!   is steered towards the target: below it one concealment frame is inserted
//!   ([`Playout::Stretch`]), well above it one frame is skipped; adjustments are rate-limited.
//!   (Re)buffering waits until the oldest packet reached the target delay. If the queue grows
//!   past `max_delay_us + frame` (bursts) the oldest frames are dropped, bounding latency.
//! * A large `seq` discontinuity (host restarted the stream) resets the buffer.

use crate::audio::AudioPacket;
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};

/// Sequence jump treated as a stream restart rather than loss / lateness.
const RESET_GAP: u64 = 500;
/// Transit samples kept for the jitter estimate.
const TRANSIT_WINDOW: usize = 128;
/// Minimum pops between two delay adjustments (stretch / accelerate).
const ADJUST_EVERY_POPS: u64 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitterConfig {
    /// Duration of one packet.
    pub frame_us: u32,
    pub min_delay_us: u32,
    pub max_delay_us: u32,
    /// Hard cap on queued packets.
    pub capacity: usize,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self { frame_us: 10_000, min_delay_us: 20_000, max_delay_us: 60_000, capacity: 64 }
    }
}

/// What to play for the next frame slot.
#[derive(Debug, Clone, PartialEq)]
pub enum Playout {
    /// The packet for this slot.
    Packet(AudioPacket),
    /// The packet for `seq` is missing: conceal it. `next` is the following packet when it is
    /// already buffered (in-band FEC source); it stays queued and is played next.
    Lost { seq: u64, next: Option<AudioPacket> },
    /// Insert one concealment frame without consuming a packet (grows the playout delay).
    Stretch,
    /// Not playing yet (initial fill / rebuffering after a stall): output silence.
    Buffering,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Accepted,
    /// Its playout slot already passed.
    Late,
    Duplicate,
    /// Sequence discontinuity: the buffer was reset and the packet starts a new stream.
    Reset,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JitterStats {
    pub received: u64,
    pub played: u64,
    /// Frame slots concealed because their packet never arrived in time.
    pub lost: u64,
    pub late: u64,
    pub duplicates: u64,
    /// Dropped to bound latency (queue above `max_delay_us`) or capacity.
    pub overflow_dropped: u64,
    /// Concealment frames inserted to grow the delay towards the target.
    pub stretched: u64,
    /// Frames skipped to shrink the delay towards the target.
    pub accelerated: u64,
    /// Times the queue ran dry while playing.
    pub underruns: u64,
    pub resets: u64,
    pub target_delay_us: u32,
    pub buffered_us: u32,
    /// Current jitter estimate (p95 of relative transit), µs.
    pub jitter_us: u32,
}

pub struct JitterBuffer {
    cfg: JitterConfig,
    packets: BTreeMap<u64, AudioPacket>,
    next_seq: Option<u64>,
    buffering: bool,
    empty_pops: u32,
    transits: VecDeque<i64>,
    min_transit: i64,
    target_us: u32,
    jitter_us: u32,
    pops: u64,
    last_adjust: u64,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        let mut cfg = cfg;
        cfg.frame_us = cfg.frame_us.max(1);
        cfg.max_delay_us = cfg.max_delay_us.max(cfg.min_delay_us);
        cfg.capacity = cfg.capacity.max(4);
        Self {
            target_us: cfg.min_delay_us,
            cfg,
            packets: BTreeMap::new(),
            next_seq: None,
            buffering: true,
            empty_pops: 0,
            transits: VecDeque::with_capacity(TRANSIT_WINDOW),
            min_transit: 0,
            jitter_us: 0,
            pops: 0,
            last_adjust: 0,
            stats: JitterStats::default(),
        }
    }

    pub fn config(&self) -> &JitterConfig {
        &self.cfg
    }

    /// Packet duration changed (e.g. the sender switched from 10 to 20 ms frames).
    pub fn set_frame_us(&mut self, frame_us: u32) {
        self.cfg.frame_us = frame_us.max(1);
    }

    pub fn target_delay_us(&self) -> u32 {
        self.target_us
    }

    pub fn is_buffering(&self) -> bool {
        self.buffering
    }

    /// Queued audio, measured as the seq span (gaps count: they will be concealed).
    pub fn buffered_us(&self) -> u32 {
        let (Some(first), Some(last)) = (self.packets.keys().next(), self.packets.keys().next_back()) else { return 0 };
        let from = self.next_seq.map_or(*first, |n| n.min(*first));
        ((last - from + 1) * self.cfg.frame_us as u64).min(u32::MAX as u64) as u32
    }

    pub fn stats(&self) -> JitterStats {
        JitterStats { target_delay_us: self.target_us, buffered_us: self.buffered_us(), jitter_us: self.jitter_us, ..self.stats.clone() }
    }

    /// Drop everything and wait for a fresh stream.
    pub fn reset(&mut self) {
        self.packets.clear();
        self.next_seq = None;
        self.buffering = true;
        self.empty_pops = 0;
        self.transits.clear();
    }

    /// Insert a packet that arrived at `arrival_us` (any monotonic receiver clock).
    pub fn push(&mut self, p: AudioPacket, arrival_us: u64) -> PushOutcome {
        let mut outcome = PushOutcome::Accepted;
        let restart = match self.next_seq {
            Some(n) => p.seq + RESET_GAP < n || p.seq >= n + RESET_GAP,
            None => self.packets.keys().next().is_some_and(|&f| p.seq + RESET_GAP < f || p.seq >= f + RESET_GAP),
        };
        if restart {
            self.reset();
            self.stats.resets += 1;
            outcome = PushOutcome::Reset;
        }
        self.observe_transit(arrival_us as i64 - p.pts_us as i64);
        if let Some(n) = self.next_seq
            && p.seq < n
        {
            self.stats.late += 1;
            return PushOutcome::Late;
        }
        if self.packets.contains_key(&p.seq) {
            self.stats.duplicates += 1;
            return PushOutcome::Duplicate;
        }
        self.packets.insert(p.seq, p);
        self.stats.received += 1;
        while self.packets.len() > self.cfg.capacity {
            self.packets.pop_first();
            self.stats.overflow_dropped += 1;
        }
        outcome
    }

    fn observe_transit(&mut self, transit: i64) {
        if self.transits.len() == TRANSIT_WINDOW {
            self.transits.pop_front();
        }
        self.transits.push_back(transit);
        let min = self.transits.iter().copied().min().unwrap_or(transit);
        self.min_transit = min;
        let mut devs: Vec<i64> = self.transits.iter().map(|t| t - min).collect();
        devs.sort_unstable();
        let p95 = devs[(devs.len() * 95 / 100).min(devs.len() - 1)].max(0) as u64;
        self.jitter_us = p95.min(u32::MAX as u64) as u32;
        let want = p95 + self.cfg.frame_us as u64;
        self.target_us = want.clamp(self.cfg.min_delay_us as u64, self.cfg.max_delay_us as u64) as u32;
    }

    /// Playout delay of a packet with `pts_us` popped at `now_us`, above the minimum transit.
    fn delay_of(&self, pts_us: u64, now_us: u64) -> i64 {
        now_us as i64 - pts_us as i64 - self.min_transit
    }

    /// The next frame slot at receiver time `now_us` (same clock as [`JitterBuffer::push`]).
    /// Call once per `frame_us` of played audio.
    pub fn pop(&mut self, now_us: u64) -> Playout {
        self.pops += 1;
        let frame = self.cfg.frame_us as i64;
        let target = self.target_us as i64;
        if self.buffering {
            let Some(first) = self.packets.values().next() else { return Playout::Buffering };
            if self.delay_of(first.pts_us, now_us) < target - frame / 2 {
                return Playout::Buffering;
            }
            let first = first.seq;
            self.buffering = false;
            self.empty_pops = 0;
            self.last_adjust = self.pops;
            self.next_seq = Some(self.next_seq.map_or(first, |n| n.max(first)));
        }
        // Hard latency bound: drop the oldest frames when the queue grew far beyond the target.
        if self.buffered_us() > self.cfg.max_delay_us + self.cfg.frame_us {
            while self.buffered_us() > self.target_us && self.packets.len() > 1 {
                if let Some((seq, _)) = self.packets.pop_first() {
                    self.stats.overflow_dropped += 1;
                    self.next_seq = Some(seq + 1);
                }
            }
            if let Some(&first) = self.packets.keys().next() {
                self.next_seq = Some(first);
            }
        }
        let n = self.next_seq.expect("playing");
        if let Some(pts) = self.packets.get(&n).map(|p| p.pts_us) {
            let delay = self.delay_of(pts, now_us);
            let may_adjust = self.pops - self.last_adjust >= ADJUST_EVERY_POPS;
            if may_adjust && delay < target - frame / 2 {
                self.last_adjust = self.pops;
                self.stats.stretched += 1;
                return Playout::Stretch;
            }
            let mut seq = n;
            if may_adjust && delay > target + 2 * frame && self.packets.contains_key(&(n + 1)) {
                self.last_adjust = self.pops;
                self.packets.remove(&n);
                self.stats.accelerated += 1;
                seq = n + 1;
            }
            let p = self.packets.remove(&seq).expect("present");
            self.next_seq = Some(seq + 1);
            self.empty_pops = 0;
            self.stats.played += 1;
            return Playout::Packet(p);
        }
        self.next_seq = Some(n + 1);
        self.stats.lost += 1;
        if self.packets.is_empty() {
            if self.empty_pops == 0 {
                self.stats.underruns += 1;
            }
            self.empty_pops += 1;
            // Conceal a short stall; after max_delay of nothing, stop and rebuffer.
            if self.empty_pops as u64 * self.cfg.frame_us as u64 >= self.cfg.max_delay_us as u64 {
                self.buffering = true;
            }
            return Playout::Lost { seq: n, next: None };
        }
        Playout::Lost { seq: n, next: self.packets.get(&(n + 1)).cloned() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioCodec;

    const FRAME: u64 = 10_000;

    fn pkt(seq: u64) -> AudioPacket {
        AudioPacket {
            seq,
            pts_us: seq * FRAME,
            codec: AudioCodec::Opus,
            sample_rate: 48_000,
            channels: 2,
            data: vec![seq as u8],
        }
    }

    /// Deterministic xorshift for synthetic network behaviour.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[derive(Default, Debug)]
    struct Sim {
        played: Vec<u64>,
        lost: Vec<u64>,
        fec_available: u64,
        silence: u64,
        stretched: u64,
    }

    /// Delivers `arrivals` (seq, arrival time) and pops one frame every 10 ms (first pop at
    /// `start_us`) until `end_us`.
    fn simulate(jb: &mut JitterBuffer, mut arrivals: Vec<(u64, u64)>, start_us: u64, end_us: u64) -> Sim {
        arrivals.sort_by_key(|a| a.1);
        let mut sim = Sim::default();
        let mut i = 0;
        let mut t = start_us;
        while t <= end_us {
            while i < arrivals.len() && arrivals[i].1 <= t {
                jb.push(pkt(arrivals[i].0), arrivals[i].1);
                i += 1;
            }
            match jb.pop(t) {
                Playout::Packet(p) => sim.played.push(p.seq),
                Playout::Stretch => sim.stretched += 1,
                Playout::Lost { seq, next } => {
                    sim.lost.push(seq);
                    sim.fec_available += next.is_some() as u64;
                }
                Playout::Buffering => sim.silence += 1,
            }
            t += FRAME;
        }
        sim
    }

    #[test]
    fn in_order_stream_plays_everything() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let arrivals = (0..100).map(|s| (s, s * FRAME + 5_000)).collect();
        let sim = simulate(&mut jb, arrivals, 0, 1_100_000);
        assert_eq!(sim.played, (0..100).collect::<Vec<_>>());
        assert!(sim.lost.iter().all(|&s| s >= 100), "only the tail after the stream ended: {:?}", sim.lost);
        assert_eq!(jb.stats().target_delay_us, 20_000, "no jitter → minimum delay");
        assert_eq!(jb.stats().late, 0);
    }

    #[test]
    fn reorders_by_sequence() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut arrivals: Vec<(u64, u64)> = (0..50).map(|s| (s, s * FRAME + 1_000)).collect();
        // 20 and 21 swapped, 33 arrives 15 ms late (still within the 20 ms target).
        arrivals[20].1 = 21 * FRAME + 1_500;
        arrivals[21].1 = 21 * FRAME + 1_000;
        arrivals[33].1 = 33 * FRAME + 16_000;
        let sim = simulate(&mut jb, arrivals, 0, 500_000);
        assert_eq!(&sim.played[..], &(0..sim.played.len() as u64).collect::<Vec<_>>()[..]);
        assert!(sim.played.len() >= 48);
        assert_eq!(jb.stats().late, 0);
    }

    #[test]
    fn loss_is_concealed_with_fec_source() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let arrivals = (0..60).filter(|s| *s != 10 && *s != 30 && *s != 31).map(|s| (s, s * FRAME)).collect();
        let sim = simulate(&mut jb, arrivals, 0, 700_000);
        assert_eq!(&sim.lost[..3], &[10, 30, 31]);
        // 10 → 11 available; 30 → 31 missing; 31 → 32 available.
        assert_eq!(sim.fec_available, 2);
        assert_eq!(sim.played.len(), 57);
        let s = jb.stats();
        assert_eq!(s.received, 57);
        assert_eq!(s.played, 57);
    }

    #[test]
    fn late_and_duplicate_packets_are_dropped() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        for s in 0..3 {
            assert_eq!(jb.push(pkt(s), s * FRAME), PushOutcome::Accepted);
        }
        assert_eq!(jb.push(pkt(1), 2 * FRAME), PushOutcome::Duplicate);
        assert!(matches!(jb.pop(3 * FRAME), Playout::Packet(p) if p.seq == 0));
        assert!(matches!(jb.pop(4 * FRAME), Playout::Packet(p) if p.seq == 1));
        assert_eq!(jb.push(pkt(0), 3 * FRAME), PushOutcome::Late);
        let s = jb.stats();
        assert_eq!((s.late, s.duplicates), (1, 1));
    }

    #[test]
    fn target_delay_adapts_to_jitter() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut rng = Rng(0x5eed);
        // Uniform 0–35 ms network jitter.
        let arrivals: Vec<(u64, u64)> = (0..400).map(|s| (s, s * FRAME + 2_000 + rng.below(35_000))).collect();
        let sim = simulate(&mut jb, arrivals, 0, 4_100_000);
        let s = jb.stats();
        assert!((38_000..=46_000).contains(&s.target_delay_us), "{s:?}");
        // After convergence, essentially nothing is late.
        let late_after_start = sim.lost.iter().filter(|&&q| (50..400).contains(&q)).count();
        assert!(late_after_start <= 8, "{late_after_start} concealed: {s:?}");
        assert!(sim.played.len() >= 380, "{s:?}");

        // Extreme jitter clamps at max_delay.
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let arrivals: Vec<(u64, u64)> = (0..300).map(|s| (s, s * FRAME + rng.below(200_000))).collect();
        simulate(&mut jb, arrivals, 0, 3_000_000);
        assert_eq!(jb.stats().target_delay_us, 60_000);
    }

    #[test]
    fn synthetic_loss_reorder_jitter_accounting() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut rng = Rng(42);
        let mut dropped = Vec::new();
        let mut arrivals = Vec::new();
        for s in 0..1000u64 {
            if rng.below(100) < 5 {
                dropped.push(s);
                continue;
            }
            // Modest jitter (≤ 8 ms) + occasional 1-slot reordering.
            let mut at = s * FRAME + 3_000 + rng.below(8_000);
            if rng.below(20) == 0 {
                at += FRAME;
            }
            arrivals.push((s, at));
        }
        let sim = simulate(&mut jb, arrivals, 0, 10_100_000);
        let s = jb.stats();
        let lost_in_stream: Vec<u64> = sim.lost.iter().copied().filter(|&s| s < 1000).collect();
        // Every dropped packet is concealed; the only other concealed slots are the few late
        // packets beyond the p95 jitter estimate.
        assert!(dropped.iter().all(|d| lost_in_stream.contains(d)));
        assert_eq!(lost_in_stream.len() as u64, dropped.len() as u64 + s.late, "{s:?}");
        assert!(s.late <= 10, "{s:?}");
        assert_eq!(sim.played.len() + lost_in_stream.len(), 1000);
        assert!(sim.played.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn stall_rebuffers_and_bursts_are_trimmed() {
        let cfg = JitterConfig::default();
        let mut jb = JitterBuffer::new(cfg.clone());
        // 0..20 normal, then a 200 ms stall after which 20..60 arrive in one burst.
        let mut arrivals: Vec<(u64, u64)> = (0..20).map(|s| (s, s * FRAME)).collect();
        arrivals.extend((20..60).map(|s| (s, 400_000)));
        arrivals.extend((60..100).map(|s| (s, s * FRAME + 200_000)));
        let sim = simulate(&mut jb, arrivals, 0, 1_400_000);
        let s = jb.stats();
        assert!(s.underruns >= 1, "{s:?}");
        assert!(s.overflow_dropped + s.late > 0, "the burst cannot all be played without growing latency: {s:?}");
        assert!(sim.played.contains(&99));
        assert!(s.buffered_us <= cfg.max_delay_us + cfg.frame_us);
    }

    #[test]
    fn sequence_restart_resets() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        for s in 5000..5005 {
            jb.push(pkt(s), s * FRAME);
        }
        jb.pop(5005 * FRAME);
        jb.pop(5006 * FRAME);
        assert_eq!(jb.push(pkt(0), 0), PushOutcome::Reset);
        assert_eq!(jb.stats().resets, 1);
        jb.push(pkt(1), FRAME);
        assert!(matches!(jb.pop(3 * FRAME), Playout::Packet(p) if p.seq == 0));
    }
}
