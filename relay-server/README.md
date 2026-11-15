# aether-relay

Rendezvous + zero-knowledge UDP relay for AetherLink (`aether-relay` binary).

* **Rendezvous** — hosts register their 9-digit device ID with an Ed25519 signature over
  `(device_id, public_key, timestamp, nonce)`. The nonce is a stateless keyed hash of the
  host's observed `ip:port` and a 30 s time bucket, so registrations cannot be replayed or
  hijacked; a key-derived ID always belongs to its key, ephemeral IDs are first-come-first-served
  until they expire (90 s TTL, refreshed every 20 s by clients).
* **Hole punching** — a lookup returns the host's reflexive + LAN addresses and at the same
  moment tells the host to punch towards the client.
* **Relay** — when no direct path works, two UDP ports are allocated per session; each peer
  latches its leg with a secret token and the server then blindly forwards datagrams between
  the two latched addresses. It only ever sees end-to-end encrypted QUIC (TLS 1.3) packets;
  authentication (pinned keys / SPAKE2 password bound to the TLS exporter) is end-to-end, so a
  malicious relay can drop traffic but cannot read or splice sessions.
* **STUN** — the main port also answers RFC 5389 Binding requests.

Wire format: `aether_protocol::rendezvous` (4-byte magic + bincode).

## Run

```sh
cargo run --release -p aether-relay -- --bind 0.0.0.0:21116 --relay-ports 30000-30999
```

| flag | default | meaning |
|---|---|---|
| `--bind` | `0.0.0.0:21116` | rendezvous/STUN UDP socket (`AETHER_RELAY_BIND`) |
| `--relay-ports` | ephemeral | `LOW-HIGH` UDP range for relay legs (2 per session) |
| `--max-sessions` | 512 | concurrent relay sessions |
| `--max-sessions-per-ip` | 8 | per requesting IP |
| `--max-registrations` | 20000 | registered devices |
| `--session-idle-secs` | 60 | close silent sessions (QUIC keep-alives every 4 s keep live ones open) |
| `--latch-timeout-secs` | 30 | both peers must latch within this time |
| `--max-session-secs` | 86400 | hard session lifetime |
| `--registration-ttl-secs` | 90 | registration expiry without refresh |
| `--rate-per-sec` / `--rate-burst` | 20 / 60 | per-IP token bucket for control packets |
| `--workers` | 2 | tokio worker threads (1 = single-threaded) |
| `--log` | `info` | log filter (or `RUST_LOG`) |

The relay legs bind on the same IP as `--bind`; clients reach them at
`<the server IP they used>:<port>`, so no public-IP flag is needed. Open UDP 21116 and the
relay range in your firewall.

## Docker

```sh
docker build -f relay-server/Dockerfile -t aether-relay .
docker run --rm -p 21116:21116/udp -p 30000-30999:30000-30999/udp aether-relay
```

The image is `FROM scratch` with a static musl binary, running as `nobody`.
Behind Docker's userland proxy, prefer `--network host` on Linux so the server observes the
clients' real source addresses (required for hole punching).

## Resource use

Idle RSS is a few MB; each relay session holds two sockets and two 2 KiB buffers, and each
registration ~150 bytes, so the default limits stay well under 15 MB.
