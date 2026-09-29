//! The profile: where F1R3Gaze keeps grants, keys, stores, cache and
//! settings, and the settings themselves (`settings.conf`, `key = value`).

use gaze_shard::ShardConfig;
use std::path::{Path, PathBuf};

pub fn default_dir() -> PathBuf {
    if let Ok(p) = std::env::var("F1R3GAZE_PROFILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/F1R3Gaze")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or(home).join("F1R3Gaze")
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share")).join("f1r3gaze")
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub home: String,
    pub shard: ShardConfig,
    /// Blob mirrors tried for every hash, after a site's own.
    pub mirrors: Vec<String>,
    /// Content cache size.
    pub cache_bytes: u64,
    pub store_quota: u64,
    /// Refuse plain-HTTP top-level documents.
    pub https_only: bool,
    /// The Embers service for wallet balances, history and transfers (the
    /// one F1R3Sky uses). Deploys go to the node directly either way.
    pub embers_api: Option<String>,
    /// Upper bound on a transfer contract's `phlo_price × phlo_limit`.
    pub max_fee: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            home: "gaze://newtab".into(),
            shard: ShardConfig::default(),
            mirrors: Vec::new(),
            cache_bytes: 512 * 1024 * 1024,
            store_quota: gaze_broker::DEFAULT_STORE_QUOTA,
            https_only: false,
            embers_api: None,
            max_fee: 10_000_000,
        }
    }
}

pub const TEMPLATE: &str = "# F1R3Gaze settings. Lists are comma-separated.
# home = gaze://newtab
# observers = https://observer-1.example, https://observer-2.example, https://observer-3.example
# validator = https://validator.example
# shard_id = root
# quorum = 2
# mirrors = https://cdn.example/blob/
# https_only = false
# Wallet balances, history and transfers (the Embers service F1R3Sky uses):
# embers_api = https://embers.example
# max_fee = 10000000
";

impl Settings {
    pub fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        let list = |v: &str| v.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "home" => s.home = v.into(),
                "observers" => s.shard.observers = list(v),
                "validator" => s.shard.validator = v.into(),
                "shard_id" => s.shard.shard_id = v.into(),
                "quorum" => s.shard.quorum = v.parse().unwrap_or(2),
                "phlo_price" => s.shard.phlo_price = v.parse().unwrap_or(1),
                "mirrors" => s.mirrors = list(v),
                "cache_bytes" => s.cache_bytes = v.parse().unwrap_or(s.cache_bytes),
                "store_quota" => s.store_quota = v.parse().unwrap_or(s.store_quota),
                "https_only" => s.https_only = v == "true",
                "embers_api" => s.embers_api = Some(v.to_string()).filter(|v| !v.is_empty()),
                "max_fee" => s.max_fee = v.parse().unwrap_or(s.max_fee),
                _ => {}
            }
        }
        s
    }

    pub fn load(dir: &Path) -> Settings {
        let p = dir.join("settings.conf");
        match std::fs::read_to_string(&p) {
            Ok(t) => Settings::parse(&t),
            Err(_) => {
                let _ = std::fs::create_dir_all(dir);
                let _ = std::fs::write(&p, TEMPLATE);
                Settings::default()
            }
        }
    }
}

/// An installation-local random user id, so site keys are per profile.
pub fn user_id(dir: &Path) -> String {
    let p = dir.join("user-id");
    if let Ok(s) = std::fs::read_to_string(&p) {
        return s.trim().to_string();
    }
    let mut b = [0u8; 16];
    let _ = getrandom::getrandom(&mut b);
    let id = gaze_net::hex(&b);
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&p, &id);
    id
}

pub fn seed() -> [u8; 32] {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).expect("OS entropy");
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_parse() {
        let s = Settings::parse("observers = https://a, https://b\nquorum=3\n# home = x\nhttps_only = true");
        assert_eq!(s.shard.observers, vec!["https://a", "https://b"]);
        assert_eq!(s.shard.quorum, 3);
        assert_eq!(s.home, "gaze://newtab");
        assert!(s.https_only);
    }
}
