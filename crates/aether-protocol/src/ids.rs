//! Identifiers: 9-digit device IDs and public-key fingerprints.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// A human-friendly 9-digit device identifier rendered as `XXX-XXX-XXX`.
///
/// Stable IDs are derived from the device's Ed25519 public key
/// ([`DeviceId::from_public_key`]); ephemeral session IDs are random.
/// Always in `100_000_000..=999_999_999` so it never has a leading zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId(u32);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeviceIdError {
    #[error("device id must contain exactly 9 digits")]
    WrongLength,
    #[error("device id contains a non-digit character")]
    NotNumeric,
    #[error("device id must not start with 0")]
    LeadingZero,
}

impl DeviceId {
    pub const MIN: u32 = 100_000_000;
    pub const MAX: u32 = 999_999_999;

    /// Construct from a raw number, validating the 9-digit range.
    pub fn new(value: u32) -> Result<Self, DeviceIdError> {
        if !(Self::MIN..=Self::MAX).contains(&value) {
            return Err(if value < Self::MIN { DeviceIdError::LeadingZero } else { DeviceIdError::WrongLength });
        }
        Ok(Self(value))
    }

    /// Deterministically derive a device ID from a 32-byte public key.
    pub fn from_public_key(public_key: &[u8; 32]) -> Self {
        let digest = blake3::derive_key("aetherlink device-id v1", public_key);
        let n = u64::from_le_bytes(digest[..8].try_into().unwrap());
        let span = (Self::MAX - Self::MIN + 1) as u64;
        Self(Self::MIN + (n % span) as u32)
    }

    /// Map 8 bytes of randomness to an ID (for ephemeral session IDs).
    pub fn from_random(bytes: [u8; 8]) -> Self {
        let span = (Self::MAX - Self::MIN + 1) as u64;
        Self(Self::MIN + (u64::from_le_bytes(bytes) % span) as u32)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0.to_string();
        write!(f, "{}-{}-{}", &s[0..3], &s[3..6], &s[6..9])
    }
}

impl FromStr for DeviceId {
    type Err = DeviceIdError;

    /// Accepts `123-456-789`, `123 456 789` or `123456789`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut digits = String::with_capacity(9);
        for c in s.chars() {
            match c {
                '0'..='9' => digits.push(c),
                '-' | ' ' | '\u{a0}' => {}
                _ => return Err(DeviceIdError::NotNumeric),
            }
        }
        if digits.len() != 9 {
            return Err(DeviceIdError::WrongLength);
        }
        if digits.starts_with('0') {
            return Err(DeviceIdError::LeadingZero);
        }
        Ok(Self(digits.parse().map_err(|_| DeviceIdError::NotNumeric)?))
    }
}

/// Short fingerprint of a public key, shown in pairing dialogs so users can
/// verify identities out-of-band (e.g. `4F2A-9C1B-77E0-D3A8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyFingerprint(pub [u8; 8]);

impl KeyFingerprint {
    pub fn of(public_key: &[u8; 32]) -> Self {
        let h = blake3::hash(public_key);
        Self(h.as_bytes()[..8].try_into().unwrap())
    }
}

impl fmt::Display for KeyFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, pair) in self.0.chunks(2).enumerate() {
            if i > 0 {
                f.write_str("-")?;
            }
            write!(f, "{:02X}{:02X}", pair[0], pair[1])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_and_parse() {
        let id = DeviceId::new(123_456_789).unwrap();
        assert_eq!(id.to_string(), "123-456-789");
        assert_eq!("123-456-789".parse::<DeviceId>().unwrap(), id);
        assert_eq!("123 456 789".parse::<DeviceId>().unwrap(), id);
        assert_eq!("123456789".parse::<DeviceId>().unwrap(), id);
        assert_eq!("12345678".parse::<DeviceId>(), Err(DeviceIdError::WrongLength));
        assert_eq!("012345678".parse::<DeviceId>(), Err(DeviceIdError::LeadingZero));
        assert_eq!("12345678x".parse::<DeviceId>(), Err(DeviceIdError::NotNumeric));
    }

    #[test]
    fn derived_ids_are_stable_and_in_range() {
        for seed in 0u8..=255 {
            let id = DeviceId::from_public_key(&[seed; 32]);
            assert!((DeviceId::MIN..=DeviceId::MAX).contains(&id.get()));
            assert_eq!(id, DeviceId::from_public_key(&[seed; 32]));
        }
        assert_ne!(DeviceId::from_public_key(&[1; 32]), DeviceId::from_public_key(&[2; 32]));
    }

    #[test]
    fn fingerprint_format() {
        let fp = KeyFingerprint::of(&[7; 32]);
        let s = fp.to_string();
        assert_eq!(s.len(), 19);
        assert_eq!(s.matches('-').count(), 3);
    }
}
