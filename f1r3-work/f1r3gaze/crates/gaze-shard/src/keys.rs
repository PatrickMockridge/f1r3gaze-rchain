//! Key custody (spec §9.1). A keystore holds named secp256k1 keys: the
//! wallets the user pays with (`wallet:<address>`), and anything else a
//! component needs to keep. The browser never writes a key anywhere else.

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
    fn load(&self, name: &str) -> Result<Option<SigningKey>, String>;
    fn store(&self, name: &str, key: &SigningKey) -> Result<(), String>;
    fn remove(&self, name: &str) -> Result<(), String>;
}

/// Keys as hex files, readable only by the user on Unix: the fallback where
/// no OS credential store is available (Linux without secret-service). File
/// names are hashes of the entry names.
pub struct FileKeystore {
    dir: PathBuf,
}

impl FileKeystore {
    pub fn new(dir: impl Into<PathBuf>) -> FileKeystore {
        FileKeystore { dir: dir.into() }
    }
    fn path(&self, name: &str) -> PathBuf {
        let h = k1ndl1ng_norm::hash::blake2b_256(name.as_bytes()).0;
        self.dir.join(format!("{}.key", gaze_net::hex(&h)))
    }
}

impl Keystore for FileKeystore {
    fn load(&self, name: &str) -> Result<Option<SigningKey>, String> {
        match std::fs::read_to_string(self.path(name)) {
            Ok(t) => {
                let b = gaze_net::unhex(t.trim()).ok_or("corrupt key file")?;
                SigningKey::from_slice(&b).map(Some).map_err(|e| e.to_string())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
    fn store(&self, name: &str, key: &SigningKey) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let p = self.path(name);
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, gaze_net::hex(&key.to_bytes())).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
    }
    fn remove(&self, name: &str) -> Result<(), String> {
        match std::fs::remove_file(self.path(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}

/// Keys in the operating system's credential store (Keychain, Windows
/// Credential Manager).
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
    fn load(&self, name: &str) -> Result<Option<SigningKey>, String> {
        let e = keyring::Entry::new(&self.service, name).map_err(|e| e.to_string())?;
        match e.get_password() {
            Ok(t) => {
                let b = gaze_net::unhex(t.trim()).ok_or("corrupt key")?;
                SigningKey::from_slice(&b).map(Some).map_err(|e| e.to_string())
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(err.to_string()),
        }
    }
    fn store(&self, name: &str, key: &SigningKey) -> Result<(), String> {
        let e = keyring::Entry::new(&self.service, name).map_err(|e| e.to_string())?;
        e.set_password(&gaze_net::hex(&key.to_bytes())).map_err(|e| e.to_string())
    }
    fn remove(&self, name: &str) -> Result<(), String> {
        let e = keyring::Entry::new(&self.service, name).map_err(|e| e.to_string())?;
        match e.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    }
}

/// A keystore in memory, for tests and for profiles that must leave nothing
/// on disk.
#[derive(Default)]
pub struct MemKeystore(std::sync::Mutex<std::collections::BTreeMap<String, [u8; 32]>>);

impl Keystore for MemKeystore {
    fn load(&self, name: &str) -> Result<Option<SigningKey>, String> {
        let m = self.0.lock().map_err(|_| "poisoned")?;
        Ok(m.get(name).and_then(|b| SigningKey::from_slice(b).ok()))
    }
    fn store(&self, name: &str, key: &SigningKey) -> Result<(), String> {
        self.0.lock().map_err(|_| "poisoned")?.insert(name.to_string(), key.to_bytes().into());
        Ok(())
    }
    fn remove(&self, name: &str) -> Result<(), String> {
        self.0.lock().map_err(|_| "poisoned")?.remove(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_by_name() {
        let dir = std::env::temp_dir().join(format!("gaze-keys-{}", std::process::id()));
        let ks = FileKeystore::new(&dir);
        assert!(ks.load("wallet:a").unwrap().is_none());
        let k = fresh_key().unwrap();
        ks.store("wallet:a", &k).unwrap();
        assert_eq!(ks.load("wallet:a").unwrap().unwrap().to_bytes(), k.to_bytes());
        assert!(ks.load("wallet:b").unwrap().is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(ks.path("wallet:a")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        ks.remove("wallet:a").unwrap();
        assert!(ks.load("wallet:a").unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
