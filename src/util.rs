//! Small helpers shared across the crate: hex, randomness, hashing,
//! password storage, and formatting.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some((high * 16 + low) as u8)
        })
        .collect()
}

pub fn decode_hex32(text: &str) -> Option<[u8; 32]> {
    decode_hex(text)?.try_into().ok()
}

/// Cryptographically random bytes from the operating system.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the operating system's random source is unavailable");
    bytes
}

/// A random token of `bytes` bytes, hex encoded.
pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("the operating system's random source is unavailable");
    hex(&buffer)
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&sha256(bytes))
}

/// HMAC-SHA256 (RFC 2104) over the sha2 crate; the textbook two-pass
/// construction, kept inline to avoid a dependency.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut inner = Sha256::new();
    inner.update(block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|byte| byte ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// PBKDF2-HMAC-SHA256 (RFC 8018) producing one 32-byte block.
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut salted = salt.to_vec();
    salted.extend_from_slice(&1u32.to_be_bytes());
    let mut block = hmac_sha256(password, &salted);
    let mut output = block;
    for _ in 1..iterations {
        block = hmac_sha256(password, &block);
        for (out, byte) in output.iter_mut().zip(block) {
            *out ^= byte;
        }
    }
    output
}

const PASSWORD_ITERATIONS: u32 = 120_000;

/// Hash a password for storage: `pbkdf2$<iterations>$<salt>$<hash>`.
pub fn hash_password(password: &str) -> String {
    let salt = random_bytes::<16>();
    let hash = pbkdf2_sha256(password.as_bytes(), &salt, PASSWORD_ITERATIONS);
    format!("pbkdf2${PASSWORD_ITERATIONS}${}${}", hex(&salt), hex(&hash))
}

/// Check a password against a stored hash in constant time.
pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    let (Some("pbkdf2"), Some(iterations), Some(salt), Some(hash), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    let (Ok(iterations), Some(salt), Some(expected)) = (
        iterations.parse::<u32>(),
        decode_hex(salt),
        decode_hex32(hash),
    ) else {
        return false;
    };
    let computed = pbkdf2_sha256(password.as_bytes(), &salt, iterations.clamp(1, 10_000_000));
    bool::from(computed.ct_eq(&expected))
}

/// Digest a bearer token for storage and comparison.
pub fn token_hash(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

/// Compare a presented token against a stored token hash in constant time.
pub fn token_matches(presented: &str, stored_hash: &str) -> bool {
    let (Some(presented), Some(stored)) = (
        decode_hex32(&token_hash(presented)),
        decode_hex32(stored_hash),
    ) else {
        return false;
    };
    bool::from(presented.ct_eq(&stored))
}

/// Compare two secrets in constant time over their digests, so lengths do
/// not leak either.
pub fn secret_eq(a: &str, b: &str) -> bool {
    bool::from(sha256(a.as_bytes()).ct_eq(&sha256(b.as_bytes())))
}

pub fn base64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn base64_standard(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()
}

pub fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
        _ => format!("{}d {}h", seconds / 86400, (seconds % 86400) / 3600),
    }
}

/// Age of a millisecond timestamp, as text.
pub fn age_of(millis: u64) -> String {
    let then = UNIX_EPOCH + Duration::from_millis(millis);
    SystemTime::now()
        .duration_since(then)
        .map(format_age)
        .unwrap_or_else(|_| "0s".to_owned())
}

pub fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// A lowercase identifier made of letters, digits, and dashes.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = true;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "project".to_owned()
    } else {
        out
    }
}

/// The user's home directory, if the platform exposes one.
pub fn home_dir() -> Option<std::path::PathBuf> {
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(variable)
        .filter(|home| !home.is_empty())
        .map(std::path::PathBuf::from)
}

/// Restrict a file to its owner. A no-op on platforms without Unix modes.
pub fn restrict(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Write a file atomically: to a sibling temporary file, then rename over.
pub fn write_atomic(path: &std::path::Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", random_hex(4)));
    std::fs::write(&temporary, contents)?;
    let _ = restrict(&temporary, mode);
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// Where activity lines go: a sink installed by the hub when one is
/// running (it broadcasts them to dashboards), stdout and stderr
/// otherwise.
static UI: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<String>> =
    std::sync::OnceLock::new();

pub fn set_activity_sink(sender: tokio::sync::mpsc::UnboundedSender<String>) {
    let _ = UI.set(sender);
}

pub fn say(line: impl Into<String>) {
    let line = line.into();
    match UI.get() {
        Some(tx) => {
            if let Err(returned) = tx.send(line) {
                println!("{}", returned.0);
            }
        }
        None => println!("{line}"),
    }
}

pub fn say_err(line: impl Into<String>) {
    let line = line.into();
    match UI.get() {
        Some(tx) => {
            if let Err(returned) = tx.send(line) {
                eprintln!("{}", returned.0);
            }
        }
        None => eprintln!("{line}"),
    }
}

/// This machine's hostname, for naming hosts.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if result == 0 {
            let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
            let name = String::from_utf8_lossy(&buffer[..end]).trim().to_owned();
            if !name.is_empty() {
                return name;
            }
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "host".to_owned())
}

/// Install the process-wide TLS crypto provider once.
pub fn init_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn pbkdf2_matches_known_vector() {
        let out = pbkdf2_sha256(b"password", b"salt", 1);
        assert_eq!(
            hex(&out),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        let out = pbkdf2_sha256(b"password", b"salt", 4096);
        assert_eq!(
            hex(&out),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    #[test]
    fn passwords_round_trip() {
        let stored = hash_password("hunter2");
        assert!(verify_password("hunter2", &stored));
        assert!(!verify_password("hunter3", &stored));
        assert!(!verify_password("hunter2", "garbage"));
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("My App!"), "my-app");
        assert_eq!(slug("--x--"), "x");
        assert_eq!(slug(""), "project");
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(decode_hex(&hex(b"\x00\xff\x10")).unwrap(), b"\x00\xff\x10");
        assert!(decode_hex("abc").is_none());
    }
}
