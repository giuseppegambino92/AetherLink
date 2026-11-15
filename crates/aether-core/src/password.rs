//! Rotating one-time session password.
//!
//! The password authenticates ephemeral-ID sessions via SPAKE2, so it only
//! needs to resist *online* guessing (attempts are rate-limited by the net
//! layer). 8 symbols from a 31-character unambiguous alphabet give ~39.6 bits.

use rand::Rng;
use std::time::{Duration, SystemTime};

/// No 0/O, 1/l/I, 5/S lookalikes.
const ALPHABET: &[u8] = b"abcdefghjkmnpqrtuvwxyz2346789";

#[derive(Debug, Clone)]
pub struct RotatingPassword {
    current: String,
    issued_at: SystemTime,
    lifetime: Duration,
}

impl RotatingPassword {
    pub fn new(lifetime: Duration) -> Self {
        Self { current: generate(), issued_at: SystemTime::now(), lifetime }
    }

    /// Current password, rotating first if it expired.
    pub fn current(&mut self) -> &str {
        if self.is_expired() {
            self.rotate();
        }
        &self.current
    }

    pub fn peek(&self) -> &str {
        &self.current
    }

    pub fn rotate(&mut self) {
        self.current = generate();
        self.issued_at = SystemTime::now();
    }

    pub fn is_expired(&self) -> bool {
        self.issued_at.elapsed().map(|e| e >= self.lifetime).unwrap_or(true)
    }

    /// Expiry as milliseconds since the Unix epoch (for the UI countdown ring).
    pub fn expires_at_ms(&self) -> u64 {
        (self.issued_at + self.lifetime)
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn lifetime(&self) -> Duration {
        self.lifetime
    }
}

/// `xxxx-xxxx`
pub fn generate() -> String {
    let mut rng = rand::thread_rng();
    let mut s = String::with_capacity(9);
    for i in 0..8 {
        if i == 4 {
            s.push('-');
        }
        s.push(ALPHABET[rng.gen_range(0..ALPHABET.len())] as char);
    }
    s
}

/// 6-digit pairing PIN for LAN pairing.
pub fn pairing_pin() -> String {
    format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format() {
        for _ in 0..200 {
            let p = generate();
            assert_eq!(p.len(), 9);
            assert_eq!(p.as_bytes()[4], b'-');
            assert!(p.bytes().filter(|&b| b != b'-').all(|b| ALPHABET.contains(&b)));
        }
        assert_eq!(pairing_pin().len(), 6);
    }

    #[test]
    fn rotation() {
        let mut p = RotatingPassword::new(Duration::from_secs(0));
        assert!(p.is_expired());
        let a = p.current().to_owned();
        assert_eq!(a.len(), 9);
        let mut q = RotatingPassword::new(Duration::from_secs(3600));
        let first = q.current().to_owned();
        assert_eq!(q.current(), first);
        q.rotate();
        // 29^8 space; collision is astronomically unlikely.
        assert_ne!(q.peek(), first);
        assert!(q.expires_at_ms() > 0);
    }
}
