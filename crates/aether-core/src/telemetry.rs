//! Host resource sampling (CPU / RAM / network) for HUD telemetry and the DVR.

use std::time::Instant;
use crate::sysinfo::{Networks, System};

pub struct HostMonitor {
    sys: System,
    nets: Networks,
    last: Instant,
    #[cfg(target_os = "android")]
    proc_ticks: Option<u64>,
}

/// Android: SELinux denies apps `/proc/stat` and `/sys/class/net`, so system-wide CPU and
/// network counters read as 0. Our own process (which includes the MediaCodec encoder and
/// the network stack work) is readable via `/proc/self/stat`: utime + stime in clock ticks.
/// Network throughput is not available to apps and stays 0.
#[cfg(target_os = "android")]
fn own_cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the `(comm)` part: state is field 3, utime 14, stime 15 (1-based).
    let rest = stat.rsplit_once(')')?.1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    Some(utime + stime)
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HostSample {
    pub cpu_percent: f32,
    pub mem_percent: f32,
    pub net_tx_kbps: u32,
    pub net_rx_kbps: u32,
}

impl Default for HostMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl HostMonitor {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        Self {
            sys,
            nets: Networks::new_with_refreshed_list(),
            last: Instant::now(),
            #[cfg(target_os = "android")]
            proc_ticks: own_cpu_ticks(),
        }
    }

    /// Sample since the previous call (call at ≥ 200 ms intervals for meaningful CPU numbers).
    pub fn sample(&mut self) -> HostSample {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.nets.refresh(true);
        let secs = self.last.elapsed().as_secs_f64().max(1e-3);
        self.last = Instant::now();
        let (tx, rx) = self
            .nets
            .iter()
            .fold((0u64, 0u64), |(t, r), (_, d)| (t + d.transmitted(), r + d.received()));
        let total = self.sys.total_memory().max(1) as f64;
        #[allow(unused_mut)]
        let mut cpu_percent = self.sys.global_cpu_usage();
        #[cfg(target_os = "android")]
        {
            // USER_HZ is fixed at 100 on Android/Linux.
            let now = own_cpu_ticks();
            if let (Some(a), Some(b)) = (self.proc_ticks, now) {
                let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
                let used = b.saturating_sub(a) as f64 / 100.0;
                cpu_percent = ((used / secs / cpus) * 100.0).clamp(0.0, 100.0) as f32;
            }
            self.proc_ticks = now;
        }
        HostSample {
            cpu_percent,
            mem_percent: (self.sys.used_memory() as f64 / total * 100.0) as f32,
            net_tx_kbps: (tx as f64 * 8.0 / 1000.0 / secs) as u32,
            net_rx_kbps: (rx as f64 * 8.0 / 1000.0 / secs) as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_sane() {
        let mut m = HostMonitor::new();
        std::thread::sleep(std::time::Duration::from_millis(250));
        let s = m.sample();
        assert!((0.0..=100.0).contains(&s.cpu_percent), "{s:?}");
        assert!(s.mem_percent > 0.0 && s.mem_percent <= 100.0, "{s:?}");
    }
}
