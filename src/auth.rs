//! Single-password login, the same scheme as Lantern: a scrypt hash in `password.lock`,
//! in-memory sessions with an absolute one-hour lifetime, and first-time setup allowed only
//! from LAN addresses. Simple on purpose: this is a small LAN service.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Same format as Lantern: `v1:<salt hex>:<hash hex>`, scrypt N=16384 r=8 p=1, 64-byte key,
/// salt used as its hex string.
const SCRYPT_LOG_N: u8 = 14;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
const KEY_LEN: usize = 64;

/// Absolute cap, not sliding: an always-open tab can't keep itself logged in forever.
pub const SESSION_TTL: Duration = Duration::from_secs(60 * 60);
pub const COOKIE: &str = "romserver_session";
pub const MIN_PASSWORD_LEN: usize = 4;

pub struct Auth {
    password_file: PathBuf,
    sessions: Mutex<HashMap<String, Instant>>,
}

impl Auth {
    pub fn new(password_file: PathBuf) -> Self {
        Self {
            password_file,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn password_file(&self) -> &Path {
        &self.password_file
    }

    pub fn is_password_set(&self) -> bool {
        self.password_file.exists()
    }

    pub fn set_password(&self, plaintext: &str) -> Result<()> {
        if self.is_password_set() {
            bail!("password already set; delete password.lock to reset it");
        }
        if plaintext.chars().count() < MIN_PASSWORD_LEN {
            bail!("password must be at least {MIN_PASSWORD_LEN} characters");
        }
        let salt = hex(&random_bytes::<32>()?);
        let hash = hash_password(plaintext, &salt)?;
        if let Some(dir) = self.password_file.parent() {
            crate::ownership::create_dir_all(dir)
                .with_context(|| format!("create {}", dir.display()))?;
        }
        write_private(&self.password_file, &format!("v1:{salt}:{hash}"))?;
        crate::ownership::own(&self.password_file)?;
        println!("[auth] password set");
        Ok(())
    }

    pub fn verify_password(&self, plaintext: &str) -> bool {
        let Ok(content) = std::fs::read_to_string(&self.password_file) else {
            return false;
        };
        let mut parts = content.trim().splitn(3, ':');
        let (Some(_version), Some(salt), Some(stored)) = (parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        match hash_password(plaintext, salt) {
            Ok(candidate) => constant_time_eq(candidate.as_bytes(), stored.as_bytes()),
            Err(_) => false,
        }
    }

    pub fn create_session(&self) -> Result<String> {
        let token = hex(&random_bytes::<32>()?);
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, expires| *expires > now);
        sessions.insert(token.clone(), now + SESSION_TTL);
        Ok(token)
    }

    pub fn validate_session(&self, token: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(token) {
            Some(expires) if *expires > Instant::now() => true,
            Some(_) => {
                sessions.remove(token);
                false
            }
            None => false,
        }
    }

    pub fn destroy_session(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }
}

/// RFC 1918 private ranges, loopback and link-local. Note that behind Docker's port mapping
/// every client appears as the bridge gateway (172.17.x.x), which counts as LAN.
pub fn is_lan(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unicast_link_local(),
    }
}

pub fn session_cookie(token: &str) -> String {
    format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SESSION_TTL.as_secs()
    )
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

pub fn token_from_cookies(header: &str) -> Option<&str> {
    header
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value)
}

fn hash_password(plaintext: &str, salt: &str) -> Result<String> {
    let params = scrypt::Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P)
        .map_err(|e| anyhow::anyhow!("scrypt params: {e}"))?;
    let mut out = [0u8; KEY_LEN];
    scrypt::scrypt(plaintext.as_bytes(), salt.as_bytes(), &params, &mut out)
        .map_err(|e| anyhow::anyhow!("scrypt: {e}"))?;
    Ok(hex(&out))
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    Ok(buf)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn write_private(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    f.write_all(content.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip_and_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let auth = Auth::new(tmp.path().join("data/password.lock"));
        assert!(!auth.is_password_set());
        assert!(auth.set_password("abc").is_err(), "too short");
        auth.set_password("hunter22").unwrap();
        assert!(auth.set_password("again1").is_err(), "only once");
        assert!(auth.verify_password("hunter22"));
        assert!(!auth.verify_password("hunter23"));

        let token = auth.create_session().unwrap();
        assert!(auth.validate_session(&token));
        auth.destroy_session(&token);
        assert!(!auth.validate_session(&token));
    }

    #[test]
    fn verifies_a_lantern_style_hash() {
        // Produced by Lantern's own code path (Node's scryptSync, same params), so the two
        // tools' password.lock files are interchangeable.
        let tmp = tempfile::tempdir().unwrap();
        let salt = "00".repeat(32);
        let hash = "f599d8ea535c0db36c898b8641da2b2e027ffd66744901e8be03d02b08ea0104781c3f0deac771604447ff5138628cc8d2b48aa8296b50ced99e0289a90d3a66";
        std::fs::write(
            tmp.path().join("password.lock"),
            format!("v1:{salt}:{hash}"),
        )
        .unwrap();
        let auth = Auth::new(tmp.path().join("password.lock"));
        assert!(auth.verify_password("lantern"));
    }

    #[test]
    fn lan_ranges() {
        for ip in [
            "192.168.1.5",
            "10.0.0.1",
            "172.17.0.1",
            "127.0.0.1",
            "::1",
            "::ffff:192.168.0.2",
        ] {
            assert!(is_lan(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "2001:db8::1"] {
            assert!(!is_lan(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn cookie_parsing() {
        assert_eq!(
            token_from_cookies("a=1; romserver_session=abc; b=2"),
            Some("abc")
        );
        assert_eq!(token_from_cookies("a=1"), None);
    }
}
