use aether_relay::{RelayConfig, RelayServer};
use clap::Parser;
use std::net::SocketAddr;
use std::time::Duration;

/// AetherLink rendezvous + zero-knowledge UDP relay server.
#[derive(Debug, Parser)]
#[command(name = "aether-relay", version, about)]
struct Args {
    /// UDP address for rendezvous/STUN traffic.
    #[arg(long, env = "AETHER_RELAY_BIND", default_value = "0.0.0.0:21116")]
    bind: SocketAddr,

    /// UDP port range for relay legs, e.g. `30000-30999` (two ports per session).
    /// Default: ephemeral ports.
    #[arg(long, env = "AETHER_RELAY_PORTS", value_parser = parse_range)]
    relay_ports: Option<(u16, u16)>,

    /// Maximum concurrent relay sessions.
    #[arg(long, env = "AETHER_RELAY_MAX_SESSIONS", default_value_t = 512)]
    max_sessions: usize,

    /// Maximum concurrent relay sessions requested from one IP.
    #[arg(long, default_value_t = 8)]
    max_sessions_per_ip: usize,

    /// Maximum number of registered devices.
    #[arg(long, default_value_t = 20_000)]
    max_registrations: usize,

    /// Close relay sessions idle for this many seconds.
    #[arg(long, default_value_t = 60)]
    session_idle_secs: u64,

    /// Both peers must latch within this many seconds.
    #[arg(long, default_value_t = 30)]
    latch_timeout_secs: u64,

    /// Hard limit on relay session lifetime (seconds).
    #[arg(long, default_value_t = 86_400)]
    max_session_secs: u64,

    /// Registration lifetime without refresh (seconds).
    #[arg(long, default_value_t = 90)]
    registration_ttl_secs: u64,

    /// Control packets per second allowed per source IP.
    #[arg(long, default_value_t = 20.0)]
    rate_per_sec: f64,

    /// Burst size of the per-IP rate limiter.
    #[arg(long, default_value_t = 60.0)]
    rate_burst: f64,

    /// Tokio worker threads (1 = single-threaded, lowest memory).
    #[arg(long, env = "AETHER_RELAY_WORKERS", default_value_t = 2)]
    workers: usize,

    /// Log filter (overridden by RUST_LOG).
    #[arg(long, default_value = "info")]
    log: String,
}

fn parse_range(s: &str) -> Result<(u16, u16), String> {
    let (a, b) = s.split_once('-').ok_or("expected LOW-HIGH")?;
    let lo: u16 = a.trim().parse().map_err(|e| format!("{e}"))?;
    let hi: u16 = b.trim().parse().map_err(|e| format!("{e}"))?;
    if lo == 0 || lo > hi {
        return Err("range must satisfy 0 < LOW <= HIGH".into());
    }
    if hi - lo < 1 {
        return Err("range must contain at least 2 ports".into());
    }
    Ok((lo, hi))
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&args.log));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();

    let cfg = RelayConfig {
        bind: args.bind,
        relay_ports: args.relay_ports,
        max_sessions: args.max_sessions,
        max_sessions_per_ip: args.max_sessions_per_ip,
        max_registrations: args.max_registrations,
        registration_ttl: Duration::from_secs(args.registration_ttl_secs),
        session_idle_timeout: Duration::from_secs(args.session_idle_secs),
        latch_timeout: Duration::from_secs(args.latch_timeout_secs),
        max_session_duration: Duration::from_secs(args.max_session_secs),
        rate_per_sec: args.rate_per_sec,
        rate_burst: args.rate_burst,
        ..RelayConfig::default()
    };

    let runtime = if args.workers <= 1 {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?
    } else {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(args.workers)
            .thread_stack_size(256 * 1024)
            .enable_all()
            .build()?
    };

    runtime.block_on(async move {
        let server = RelayServer::bind(cfg).await?;
        tokio::select! {
            r = server.run() => r,
            _ = shutdown_signal() => {
                tracing::info!("shutting down");
                Ok(())
            }
        }
    })
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
