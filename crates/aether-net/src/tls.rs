//! rustls configuration: self-signed Ed25519 certificates bound to the device identity and
//! verifiers that accept any such certificate (after checking the TLS 1.3 CertificateVerify
//! signature) while exposing the peer's Ed25519 key. Trust decisions happen post-TLS.

use crate::{Identity, NetError, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::sync::Arc;

/// SNI used by clients; the server ignores it (identity is the Ed25519 key, not a name).
pub(crate) const SERVER_NAME: &str = "aetherlink.peer";

/// TLS exporter label for binding SPAKE2 to the TLS session.
pub(crate) const EXPORTER_LABEL: &[u8] = b"EXPORTER-aetherlink-spake2-v1";

pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// PKCS#8 v2 (RFC 8410 / 5958) encoding of an Ed25519 key, as produced by ring.
fn ed25519_pkcs8(id: &Identity) -> Vec<u8> {
    let mut v = Vec::with_capacity(85);
    v.extend_from_slice(&[
        0x30, 0x53, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
    ]);
    v.extend_from_slice(&id.secret_bytes());
    // [1] publicKey, constructed, wrapping a BIT STRING (ring's v2 layout).
    v.extend_from_slice(&[0xa1, 0x23, 0x03, 0x21, 0x00]);
    v.extend_from_slice(&id.public_key());
    v
}

/// Self-signed certificate whose SubjectPublicKeyInfo is the identity's Ed25519 key.
pub(crate) fn self_signed_cert(id: &Identity) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let pkcs8 = PrivatePkcs8KeyDer::from(ed25519_pkcs8(id));
    let key_pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &rcgen::PKCS_ED25519)
        .map_err(|e| NetError::Config(format!("key: {e}")))?;
    let mut params = rcgen::CertificateParams::new(vec![SERVER_NAME.to_string()])
        .map_err(|e| NetError::Config(format!("cert params: {e}")))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, format!("AetherLink {}", id.device_id()));
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2124, 1, 1);
    let cert = params.self_signed(&key_pair).map_err(|e| NetError::Config(format!("cert: {e}")))?;
    Ok((cert.der().clone(), PrivateKeyDer::Pkcs8(pkcs8)))
}

// ---------------------------------------------------------------------------------------------
// Minimal DER walk to the SubjectPublicKeyInfo of an X.509 certificate.
// ---------------------------------------------------------------------------------------------

/// Returns (tag, content, rest).
fn der_tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let mut len = 0usize;
        for &b in &rest[..n] {
            len = (len << 8) | b as usize;
        }
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

const ED25519_ALG_ID: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];

/// Extract the Ed25519 public key from a DER certificate's SubjectPublicKeyInfo
/// (`None` if the certificate is malformed or not an Ed25519 certificate).
pub(crate) fn ed25519_key_from_cert(der: &[u8]) -> Option<[u8; 32]> {
    let (tag, cert, _) = der_tlv(der)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, tbs, _) = der_tlv(cert)?;
    if tag != 0x30 {
        return None;
    }
    let mut rest = tbs;
    // Optional explicit [0] version.
    let (tag, _, r) = der_tlv(rest)?;
    if tag == 0xA0 {
        rest = r;
    }
    // serialNumber, signature, issuer, validity, subject
    for expected in [0x02u8, 0x30, 0x30, 0x30, 0x30] {
        let (tag, _, r) = der_tlv(rest)?;
        if tag != expected {
            return None;
        }
        rest = r;
    }
    let (tag, spki, _) = der_tlv(rest)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, alg, r) = der_tlv(spki)?;
    if tag != 0x30 || alg != ED25519_ALG_ID {
        return None;
    }
    let (tag, bits, r2) = der_tlv(r)?;
    if tag != 0x03 || !r2.is_empty() || bits.len() != 33 || bits[0] != 0 {
        return None;
    }
    bits[1..].try_into().ok()
}

// ---------------------------------------------------------------------------------------------
// Verifiers
// ---------------------------------------------------------------------------------------------

fn check_cert(end_entity: &CertificateDer<'_>) -> std::result::Result<(), rustls::Error> {
    ed25519_key_from_cert(end_entity)
        .map(|_| ())
        .ok_or(rustls::Error::InvalidCertificate(CertificateError::BadEncoding))
}

#[derive(Debug)]
struct AcceptAnyEd25519 {
    algs: WebPkiSupportedAlgorithms,
}

impl AcceptAnyEd25519 {
    fn new(provider: &CryptoProvider) -> Self {
        Self { algs: provider.signature_verification_algorithms }
    }

    fn verify13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        if dss.scheme != SignatureScheme::ED25519 {
            return Err(rustls::Error::PeerMisbehaved(rustls::PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme));
        }
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }
}

impl ServerCertVerifier for AcceptAnyEd25519 {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        check_cert(end_entity).map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.verify13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

impl ClientCertVerifier for AcceptAnyEd25519 {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, rustls::Error> {
        check_cert(end_entity).map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.verify13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

pub(crate) fn server_crypto(id: &Identity) -> Result<rustls::ServerConfig> {
    let provider = provider();
    let (cert, key) = self_signed_cert(id)?;
    let verifier = Arc::new(AcceptAnyEd25519::new(&provider));
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| NetError::Config(e.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![cert], key)
        .map_err(|e| NetError::Config(e.to_string()))?;
    cfg.alpn_protocols = vec![aether_protocol::ALPN.to_vec()];
    cfg.max_early_data_size = 0;
    Ok(cfg)
}

pub(crate) fn client_crypto(id: &Identity) -> Result<rustls::ClientConfig> {
    let provider = provider();
    let (cert, key) = self_signed_cert(id)?;
    let verifier = Arc::new(AcceptAnyEd25519::new(&provider));
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| NetError::Config(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| NetError::Config(e.to_string()))?;
    cfg.alpn_protocols = vec![aether_protocol::ALPN.to_vec()];
    cfg.enable_early_data = false;
    Ok(cfg)
}

/// Ed25519 key of the authenticated TLS peer of a quinn connection.
pub(crate) fn peer_key(conn: &quinn::Connection) -> Result<[u8; 32]> {
    let ident = conn.peer_identity().ok_or_else(|| NetError::Protocol("peer presented no certificate".into()))?;
    let certs = ident
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| NetError::Protocol("unexpected peer identity type".into()))?;
    let first = certs.first().ok_or_else(|| NetError::Protocol("empty certificate chain".into()))?;
    ed25519_key_from_cert(first).ok_or_else(|| NetError::Protocol("peer certificate is not Ed25519".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_carries_identity_key() {
        let id = Identity::generate();
        let (cert, _) = self_signed_cert(&id).unwrap();
        assert_eq!(ed25519_key_from_cert(&cert), Some(id.public_key()));
        // Truncations never panic and never yield a key.
        for n in 0..cert.len() {
            assert_eq!(ed25519_key_from_cert(&cert[..n]), None);
        }
    }

    #[test]
    fn configs_build() {
        let id = Identity::generate();
        server_crypto(&id).unwrap();
        client_crypto(&id).unwrap();
    }
}
