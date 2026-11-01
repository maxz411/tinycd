//! The hub's own certificate: self-signed, generated once, and identified
//! to agents by its SHA-256 fingerprint rather than by a certificate
//! authority. Agents learn the fingerprint from the join token.

use std::path::Path;

use crate::util::{self, Error};

#[derive(Clone)]
pub struct TlsIdentity {
    pub cert_pem: String,
    pub key_pem: String,
    /// Hex SHA-256 of the certificate's DER encoding.
    pub fingerprint: String,
}

/// Load the certificate under `dir`, generating it on first use.
pub fn ensure(dir: &Path, names: &[String]) -> Result<TlsIdentity, Error> {
    std::fs::create_dir_all(dir)?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    if let (Ok(cert_pem), Ok(key_pem)) = (
        std::fs::read_to_string(&cert_path),
        std::fs::read_to_string(&key_path),
    ) && let Some(der) = pem_to_der(&cert_pem)
    {
        return Ok(TlsIdentity {
            fingerprint: util::sha256_hex(&der),
            cert_pem,
            key_pem,
        });
    }

    util::init_crypto();
    let mut subject_alt_names: Vec<String> = vec!["localhost".to_owned(), util::hostname()];
    subject_alt_names.extend(names.iter().cloned());
    subject_alt_names.dedup();
    let certified = rcgen::generate_simple_self_signed(subject_alt_names)
        .map_err(|error| format!("failed to generate a certificate: {error}"))?;
    let identity = TlsIdentity {
        fingerprint: util::sha256_hex(certified.cert.der()),
        cert_pem: certified.cert.pem(),
        key_pem: certified.signing_key.serialize_pem(),
    };
    util::write_atomic(&cert_path, identity.cert_pem.as_bytes(), 0o644)?;
    util::write_atomic(&key_path, identity.key_pem.as_bytes(), 0o600)?;
    Ok(identity)
}

/// The DER bytes of the first block in a PEM file.
pub fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    let mut body = String::new();
    let mut inside = false;
    for line in pem.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN") {
            inside = true;
        } else if line.starts_with("-----END") {
            break;
        } else if inside {
            body.push_str(line);
        }
    }
    util::base64_decode(&body).filter(|der| !der.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificates_persist_with_a_stable_fingerprint() {
        let dir = std::env::temp_dir().join(format!("tinycd-tls-{}", util::random_hex(4)));
        let first = ensure(&dir, &["example.test".to_owned()]).unwrap();
        let second = ensure(&dir, &[]).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.fingerprint.len(), 64);
        assert!(first.cert_pem.contains("BEGIN CERTIFICATE"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
