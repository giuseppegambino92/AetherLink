//! Resource-spike detection on telemetry samples.

use aether_protocol::TelemetrySample;

/// Thresholds for [`detect_spike`].
#[derive(Debug, Clone, PartialEq)]
pub struct SpikeConfig {
    /// Absolute host CPU threshold (percent); fires on the rising edge.
    pub cpu_percent: f32,
    /// Absolute host memory threshold (percent); fires on the rising edge.
    pub mem_percent: f32,
    /// A value more than `sigma` standard deviations above the recent mean is a spike.
    pub sigma: f32,
    /// Number of recent samples used for mean/σ.
    pub history: usize,
    /// Minimum history before σ-based detection kicks in.
    pub min_history: usize,
    /// Minimum time between two automatic spike marks.
    pub cooldown_us: u64,
}

impl Default for SpikeConfig {
    fn default() -> Self {
        Self {
            cpu_percent: 85.0,
            mem_percent: 90.0,
            sigma: 3.0,
            history: 60,
            min_history: 10,
            cooldown_us: 5_000_000,
        }
    }
}

struct Metric {
    name: &'static str,
    unit: &'static str,
    get: fn(&TelemetrySample) -> f64,
    /// Smallest standard deviation assumed (avoids firing on noise of a flat series).
    min_std: fn(f64) -> f64,
}

const METRICS: [Metric; 4] = [
    Metric {
        name: "CPU",
        unit: "%",
        get: |s| s.host_cpu_percent as f64,
        min_std: |_| 2.0,
    },
    Metric {
        name: "memory",
        unit: "%",
        get: |s| s.host_mem_percent as f64,
        min_std: |_| 1.0,
    },
    Metric {
        name: "network",
        unit: " kbps",
        get: |s| s.host_net_tx_kbps as f64 + s.host_net_rx_kbps as f64,
        min_std: |mean| (mean * 0.05).max(100.0),
    },
    Metric {
        name: "RTT",
        unit: " ms",
        get: |s| s.rtt_ms as f64,
        min_std: |mean| (mean * 0.05).max(5.0),
    },
];

/// Decide whether `sample` is a resource spike given the preceding `history` (oldest
/// first). Returns a human-readable label for the `ResourceSpike` timeline mark.
///
/// Fires when CPU/memory cross their absolute thresholds (rising edge), or when CPU,
/// memory, network throughput or RTT jump more than `sigma` standard deviations above
/// the mean of the last `cfg.history` samples.
pub fn detect_spike(
    history: &[TelemetrySample],
    sample: &TelemetrySample,
    cfg: &SpikeConfig,
) -> Option<String> {
    let prev = history.last();
    if sample.host_cpu_percent > cfg.cpu_percent
        && prev.is_none_or(|p| p.host_cpu_percent <= cfg.cpu_percent)
    {
        return Some(format!("CPU {:.0}%", sample.host_cpu_percent));
    }
    if sample.host_mem_percent > cfg.mem_percent
        && prev.is_none_or(|p| p.host_mem_percent <= cfg.mem_percent)
    {
        return Some(format!("memory {:.0}%", sample.host_mem_percent));
    }
    if history.len() < cfg.min_history.max(2) {
        return None;
    }
    let window = &history[history.len().saturating_sub(cfg.history)..];
    for m in &METRICS {
        let n = window.len() as f64;
        let mean = window.iter().map(m.get).sum::<f64>() / n;
        let var = window
            .iter()
            .map(|s| ((m.get)(s) - mean).powi(2))
            .sum::<f64>()
            / n;
        let std = var.sqrt().max((m.min_std)(mean));
        let v = (m.get)(sample);
        if v > mean + cfg.sigma as f64 * std {
            return Some(format!(
                "{} spike: {:.0}{} (avg {:.0}{})",
                m.name, v, m.unit, mean, m.unit
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(cpu: f32, net: u32) -> TelemetrySample {
        TelemetrySample {
            host_cpu_percent: cpu,
            host_mem_percent: 40.0,
            host_net_tx_kbps: net,
            rtt_ms: 20.0,
            ..Default::default()
        }
    }

    #[test]
    fn threshold_rising_edge() {
        let cfg = SpikeConfig::default();
        assert_eq!(
            detect_spike(&[], &s(90.0, 0), &cfg).as_deref(),
            Some("CPU 90%")
        );
        assert!(
            detect_spike(&[s(90.0, 0)], &s(92.0, 0), &cfg).is_none(),
            "already above: no new spike"
        );
        assert!(detect_spike(&[s(50.0, 0)], &s(60.0, 0), &cfg).is_none());
    }

    #[test]
    fn sigma_jump() {
        let cfg = SpikeConfig::default();
        let hist: Vec<_> = (0..30)
            .map(|i| s(20.0 + (i % 3) as f32, 1000 + (i % 5) * 10))
            .collect();
        assert!(detect_spike(&hist, &s(22.0, 1010), &cfg).is_none());
        let label = detect_spike(&hist, &s(21.0, 9000), &cfg).unwrap();
        assert!(label.starts_with("network spike"), "{label}");
        let label = detect_spike(&hist, &s(60.0, 1000), &cfg).unwrap();
        assert!(label.starts_with("CPU spike"), "{label}");
        // Not enough history for σ detection.
        assert!(detect_spike(&hist[..3], &s(60.0, 1000), &cfg).is_none());
    }
}
