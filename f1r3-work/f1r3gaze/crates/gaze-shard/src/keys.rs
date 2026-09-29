//! Key custody (spec §9.1). Each (user, site) pair gets its own secp256k1
//! key, generated on first grant, so sites cannot correlate a user by key.
//! Session keys are made fresh per session and never stored.

use k256::ecdsa::SigningKey;
use std::path::PathBuf;

pub fn fresh_key() -> Result<SigningKey, String> {
    loop {
        let mut b = [0u8; 32];
        getrandom::getrandom(&mut b).map_err(|e| e.to_string())?;
        if let Ok(k) = SigningKey::from_slice(&b) {
            return Ok(k);
        }
    }
}

pub trait Keystore: Send + Sync + 'static {
    /// The key for `site` under `user`, created if absent.
    fn site_key(&self, user: &str, site: &str) -> Result<SigningKey, String>;
    /// Forget a site's key (settings: "forget this site").
    fn forget(&self, user: &str, site: &str) -> Result<(), String>;
}

fn entry_name(user: &str, site: &str) -> String {
    let h = k1ndl1ng_norm::hash::blake2b_256(format!("{user}\n{site}").as_bytes()).0;
    gaze_net::hex(&h)
}

/// Keys as hex files, one per (user, site), readable only by the user on
/// Unix. The fallback where no OS credential store is available.
pub struct FileKeystore {
    dir: PathBuf,
}

impl FileKeystore {
    pub fn new(dir: impl Into<PathBuf>) -> FileKeystore {
        FileKeystore { dir: dir.into() }
    }
}

impl Keystore for FileKeystore {
    fn site_key(&self, user: &str, site: &str) -> Result<SigningKey, String> {
        let p = self.dir.join(format!("{}.key", entry_name(user, site)));
        if let Ok(t) = std::fs::read_to_string(&p) {
            let b = gaze_net::unhex(t.trim()).ok_or("corrupt key file")?;
            return SigningKey::from_slice(&b).map_err(|e| e.to_string());
        }
        let k = fresh_key()?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, gaze_net::hex(&k.to_bytes())).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, &p).map_err(|e| e.to_string())?;
        Ok(k)
    }
    fn forget(&self, user: &str, site: &str) -> Result<(), String> {
        let p = self.dir.join(format!("{}.key", entry_name(user, site)));
        match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}

/// Keys in the operating system's credential store.
#[cfg(feature = "os-keyring")]
pub struct OsKeystore {
    service: String,
}

#[cfg(feature = "os-keyring")]
impl OsKeystore {
    pub fn new(service: &str) -> OsKeystore {
        OsKeystore { service: service.into() }
    }
}

#[cfg(feature = "os-keyring")]
impl Keystore for OsKeystore {
    fn site_key(&self, user: &str, site: &str) -> Result<SigningKey, String> {
        let e = keyring::Entry::new(&self.service, &entry_name(user, site)).map_err(|e| e.to_string())?;
        match e.get_password() {
            Ok(t) => {
                let b = gaze_net::unhex(t.trim()).ok_or("corrupt key")?;
                SigningKey::from_slice(&b).map_err(|e| e.to_string())
            }
            Err(keyring::Error::NoEntry) => {
                let k = fresh_key()?;
                e.set_password(&gaze_net::hex(&k.to_bytes())).map_err(|e| e.to_string())?;
                Ok(k)
            }
            Err(err) => Err(err.to_string()),
        }
    }
    fn forget(&self, user: &str, site: &str) -> Result<(), String> {
        let e = keyring::Entry::new(&self.service, &entry_name(user, site)).map_err(|e| e.to_string())?;
        match e.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_key_per_site_and_stable() {
        let dir = std::env::temp_dir().join(format!("gaze-keys-{}", std::process::id()));
        let ks = FileKeystore::new(&dir);
        let a = ks.site_key("u", "https://a.example:443").unwrap();
        let b = ks.site_key("u", "https://b.example:443").unwrap();
        assert_ne!(a.to_bytes(), b.to_bytes());
        assert_eq!(ks.site_key("u", "https://a.example:443").unwrap().to_bytes(), a.to_bytes());
        ks.forget("u", "https://a.example:443").unwrap();
        assert_ne!(ks.site_key("u", "https://a.example:443").unwrap().to_bytes(), a.to_bytes());
        let _ = std::fs::remove_dir_all(dir);
    }
}
