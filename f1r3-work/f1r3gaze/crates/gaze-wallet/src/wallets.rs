//! The profile's wallets. Keys live in the keystore (`wallet:<address>`);
//! `wallets.tsv` lists addresses and labels, and `wallet-active` names the
//! wallet that pays. The first wallet created or imported becomes active.

use crate::address::Address;
use crate::embers::{Embers, WalletState};
use crate::file;
use gaze_shard::keys::fresh_key;
use gaze_shard::{Keystore, Payer};
use k256::ecdsa::SigningKey;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub address: Address,
    pub label: String,
}

struct Inner {
    list: Vec<Entry>,
    active: Option<Address>,
    balance: Option<(Instant, Address, u64)>,
}

pub struct Wallets {
    dir: PathBuf,
    keys: Arc<dyn Keystore>,
    pub embers: Option<Embers>,
    inner: Mutex<Inner>,
}

fn clean(label: &str) -> String {
    label.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

fn entry_name(a: &Address) -> String {
    format!("wallet:{a}")
}

impl Wallets {
    pub fn open(dir: impl Into<PathBuf>, keys: Arc<dyn Keystore>, embers: Option<Embers>) -> Wallets {
        let dir = dir.into();
        let list = std::fs::read_to_string(dir.join("wallets.tsv"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let (a, label) = l.split_once('\t').unwrap_or((l, ""));
                Some(Entry {
                    address: Address::parse(a).ok()?,
                    label: label.to_string(),
                })
            })
            .collect::<Vec<_>>();
        let active = std::fs::read_to_string(dir.join("wallet-active"))
            .ok()
            .and_then(|a| Address::parse(&a).ok())
            .filter(|a| list.iter().any(|e| &e.address == a));
        Wallets {
            dir,
            keys,
            embers,
            inner: Mutex::new(Inner {
                list,
                active,
                balance: None,
            }),
        }
    }

    fn save(&self, i: &Inner) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let body: String = i.list.iter().map(|e| format!("{}\t{}\n", e.address, e.label)).collect();
        std::fs::write(self.dir.join("wallets.tsv"), body).map_err(|e| e.to_string())?;
        match &i.active {
            Some(a) => std::fs::write(self.dir.join("wallet-active"), a.as_str()).map_err(|e| e.to_string()),
            None => {
                let _ = std::fs::remove_file(self.dir.join("wallet-active"));
                Ok(())
            }
        }
    }

    fn add(&self, key: &SigningKey, label: &str) -> Result<Address, String> {
        let a = Address::from_key(key.verifying_key());
        self.keys.store(&entry_name(&a), key)?;
        let mut i = self.inner.lock().map_err(|_| "poisoned")?;
        match i.list.iter_mut().find(|e| e.address == a) {
            Some(e) if !label.is_empty() => e.label = clean(label),
            Some(_) => {}
            None => i.list.push(Entry {
                address: a.clone(),
                label: clean(label),
            }),
        }
        if i.active.is_none() {
            i.active = Some(a.clone());
        }
        self.save(&i)?;
        Ok(a)
    }

    pub fn create(&self, label: &str) -> Result<Address, String> {
        self.add(&fresh_key()?, label)
    }

    /// Import a wallet file saved by F1R3Sky (or a bare hex key).
    pub fn import(&self, text: &str, label: &str) -> Result<Address, String> {
        self.add(&file::deserialize(text)?, label)
    }

    /// The wallet file for `a`, loadable by F1R3Sky.
    pub fn export(&self, a: &Address) -> Result<String, String> {
        Ok(file::serialize(&self.key(a)?))
    }

    pub fn key(&self, a: &Address) -> Result<SigningKey, String> {
        self.keys.load(&entry_name(a))?.ok_or_else(|| format!("no key for wallet {a}"))
    }

    /// Forget a wallet: its key is deleted from the keystore. Export it
    /// first if its funds matter.
    pub fn remove(&self, a: &Address) -> Result<(), String> {
        self.keys.remove(&entry_name(a))?;
        let mut i = self.inner.lock().map_err(|_| "poisoned")?;
        i.list.retain(|e| &e.address != a);
        if i.active.as_ref() == Some(a) {
            i.active = i.list.first().map(|e| e.address.clone());
        }
        i.balance = None;
        self.save(&i)
    }

    pub fn set_active(&self, a: &Address) -> Result<(), String> {
        let mut i = self.inner.lock().map_err(|_| "poisoned")?;
        if !i.list.iter().any(|e| &e.address == a) {
            return Err(format!("no wallet {a}"));
        }
        i.active = Some(a.clone());
        i.balance = None;
        self.save(&i)
    }

    pub fn list(&self) -> Vec<(Entry, bool)> {
        let i = match self.inner.lock() {
            Ok(i) => i,
            Err(_) => return Vec::new(),
        };
        i.list.iter().map(|e| (e.clone(), Some(&e.address) == i.active.as_ref())).collect()
    }

    pub fn active(&self) -> Option<Address> {
        self.inner.lock().ok()?.active.clone()
    }

    pub fn state(&self, a: &Address) -> Result<WalletState, String> {
        let e = self.embers.as_ref().ok_or("no Embers service is configured (settings: embers_api)")?;
        let s = e.state(a)?;
        if let Ok(mut i) = self.inner.lock() {
            i.balance = Some((Instant::now(), a.clone(), s.balance));
        }
        Ok(s)
    }

    /// Transfer from wallet `from`. The prepared contract is checked before
    /// the key signs it. Returns the deploy id.
    pub fn transfer(&self, from: &Address, to: &Address, amount: i64, description: Option<&str>) -> Result<String, String> {
        let e = self.embers.as_ref().ok_or("no Embers service is configured (settings: embers_api)")?;
        let id = e.transfer(&self.key(from)?, to, amount, description)?;
        if let Ok(mut i) = self.inner.lock() {
            i.balance = None;
        }
        Ok(id)
    }
}

impl Payer for Wallets {
    fn payer(&self) -> Result<(SigningKey, String), String> {
        let a = self
            .active()
            .ok_or("no wallet to pay with: create or import one (Wallet panel, or `f1r3gaze wallet new`)")?;
        Ok((self.key(&a)?, a.as_str().to_string()))
    }

    fn balance(&self) -> Option<u64> {
        let a = self.active()?;
        if let Ok(i) = self.inner.lock() {
            if let Some((t, ba, v)) = &i.balance {
                if ba == &a && t.elapsed() < Duration::from_secs(15) {
                    return Some(*v);
                }
            }
        }
        self.state(&a).ok().map(|s| s.balance)
    }
}
