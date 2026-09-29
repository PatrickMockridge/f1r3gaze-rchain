//! `gaze-store` — the `store` capability (spec §8.2).
//!
//! One append-only file per origin. Records:
//!
//! ```text
//! 0x01 put  LEB key-len, key (UTF-8), LEB value-len, value (K1G encoding)
//! 0x02 del  LEB key-len, key
//! ```
//!
//! On open the file is replayed into memory; a torn final record (a crash
//! mid-append) is dropped. When dead bytes exceed live bytes plus 64 KiB the
//! file is compacted by writing a fresh one and renaming it over the old.
//!
//! Values are any closed normal form, except that unforgeable names are
//! refused anywhere inside them: a name must not outlive the tab that minted
//! it. Keys are strings of at most 1 KiB. The quota counts key and value
//! bytes.

#![forbid(unsafe_code)]

use k1ndl1ng_norm::term::leb;
use k1ndl1ng_norm::{Name, Node, Norm};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAX_KEY: usize = 1024;
const COMPACT_SLACK: u64 = 64 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    Quota,
    Key,
    /// The value holds an unforgeable name.
    Unforgeable,
    Io(String),
}

impl StoreError {
    pub fn code(&self) -> &'static str {
        match self {
            StoreError::Quota => "quota",
            StoreError::Key | StoreError::Unforgeable => "type",
            StoreError::Io(_) => "io",
        }
    }
}

/// Does a term contain an unforgeable name anywhere, including inside quotes?
pub fn has_unforgeable(t: &Norm) -> bool {
    let mut procs = vec![t.clone()];
    let mut names: Vec<Name> = Vec::new();
    loop {
        while let Some(n) = names.pop() {
            match n {
                Name::Unforgeable(_) => return true,
                Name::Quote(q) => procs.push(q),
                _ => {}
            }
        }
        let Some(p) = procs.pop() else { return false };
        match p.node() {
            Node::Par(v) => procs.extend(v.iter().cloned()),
            Node::Send { chan, args, .. } => {
                names.push(chan.clone());
                procs.extend(args.iter().cloned());
            }
            Node::Receive { binds, body } => {
                for b in binds {
                    names.push(b.chan.clone());
                }
                procs.push(body.clone());
            }
            Node::New { body, .. } => procs.push(body.clone()),
            Node::Eval(n) => names.push(n.clone()),
            Node::Coll { items, .. } => procs.extend(items.iter().cloned()),
            _ => {}
        }
    }
}

pub struct OriginStore {
    path: PathBuf,
    file: File,
    map: BTreeMap<String, Vec<u8>>,
    live: u64,
    file_len: u64,
    quota: u64,
}

fn read_leb(b: &[u8], i: &mut usize) -> Option<usize> {
    let mut out = 0usize;
    for s in 0..5 {
        let c = *b.get(*i)?;
        *i += 1;
        out |= ((c & 0x7f) as usize) << (7 * s);
        if c & 0x80 == 0 {
            return Some(out);
        }
    }
    None
}

fn io(e: std::io::Error) -> StoreError {
    StoreError::Io(e.to_string())
}

impl OriginStore {
    pub fn open(path: impl AsRef<Path>, quota: u64) -> Result<OriginStore, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(io)?;
        }
        let bytes = std::fs::read(&path).unwrap_or_default();
        let mut map = BTreeMap::new();
        let mut i = 0;
        let mut good = 0;
        while i < bytes.len() {
            let mut j = i;
            let op = bytes[j];
            j += 1;
            let rec = (|| {
                let kl = read_leb(&bytes, &mut j)?;
                let k = std::str::from_utf8(bytes.get(j..j + kl)?).ok()?.to_string();
                j += kl;
                match op {
                    1 => {
                        let vl = read_leb(&bytes, &mut j)?;
                        let v = bytes.get(j..j + vl)?.to_vec();
                        j += vl;
                        Some((k, Some(v)))
                    }
                    2 => Some((k, None)),
                    _ => None,
                }
            })();
            match rec {
                Some((k, Some(v))) => {
                    map.insert(k, v);
                }
                Some((k, None)) => {
                    map.remove(&k);
                }
                None => break, // torn tail
            }
            i = j;
            good = j;
        }
        if good < bytes.len() {
            // Drop the torn tail before appending more.
            let f = OpenOptions::new().write(true).open(&path).map_err(io)?;
            f.set_len(good as u64).map_err(io)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path).map_err(io)?;
        let live = map.iter().map(|(k, v)| (k.len() + v.len()) as u64).sum();
        Ok(OriginStore {
            path,
            file,
            map,
            live,
            file_len: good as u64,
            quota,
        })
    }

    pub fn get(&self, k: &str) -> Option<Norm> {
        Norm::decode(self.map.get(k)?).ok()
    }

    pub fn list(&self, prefix: &str) -> Vec<String> {
        self.map.range(prefix.to_string()..).take_while(|(k, _)| k.starts_with(prefix)).map(|(k, _)| k.clone()).collect()
    }

    pub fn used(&self) -> u64 {
        self.live
    }

    pub fn put(&mut self, k: &str, v: &Norm) -> Result<(), StoreError> {
        if k.is_empty() || k.len() > MAX_KEY {
            return Err(StoreError::Key);
        }
        if has_unforgeable(v) {
            return Err(StoreError::Unforgeable);
        }
        let enc = v.encode();
        let old = self.map.get(k).map(|o| (k.len() + o.len()) as u64).unwrap_or(0);
        let new_live = self.live - old + (k.len() + enc.len()) as u64;
        if new_live > self.quota {
            return Err(StoreError::Quota);
        }
        let mut rec = vec![1u8];
        leb(k.len() as u32, &mut rec);
        rec.extend_from_slice(k.as_bytes());
        leb(enc.len() as u32, &mut rec);
        rec.extend_from_slice(enc);
        self.append(&rec)?;
        self.map.insert(k.to_string(), enc.to_vec());
        self.live = new_live;
        self.maybe_compact()
    }

    pub fn del(&mut self, k: &str) -> Result<(), StoreError> {
        let Some(old) = self.map.remove(k) else { return Ok(()) };
        self.live -= (k.len() + old.len()) as u64;
        let mut rec = vec![2u8];
        leb(k.len() as u32, &mut rec);
        rec.extend_from_slice(k.as_bytes());
        self.append(&rec)?;
        self.maybe_compact()
    }

    fn append(&mut self, rec: &[u8]) -> Result<(), StoreError> {
        self.file.write_all(rec).map_err(io)?;
        self.file.flush().map_err(io)?;
        self.file_len += rec.len() as u64;
        Ok(())
    }

    fn maybe_compact(&mut self) -> Result<(), StoreError> {
        if self.file_len <= 2 * self.live + COMPACT_SLACK {
            return Ok(());
        }
        let mut out = Vec::new();
        for (k, v) in &self.map {
            out.push(1u8);
            leb(k.len() as u32, &mut out);
            out.extend_from_slice(k.as_bytes());
            leb(v.len() as u32, &mut out);
            out.extend_from_slice(v);
        }
        let tmp = self.path.with_extension("compact");
        std::fs::write(&tmp, &out).map_err(io)?;
        std::fs::rename(&tmp, &self.path).map_err(io)?;
        self.file = OpenOptions::new().append(true).open(&self.path).map_err(io)?;
        self.file_len = out.len() as u64;
        Ok(())
    }

    /// Serve one page request: `("get", k, ret)`, `("put", k, v, ack)`,
    /// `("del", k, ack)`, `("list", prefix, ret)`. Returns the reply datum
    /// for the last argument's name, if the request carried one.
    pub fn serve(&mut self, args: &[Norm]) -> Option<Norm> {
        let ok = |v: Norm| Norm::tuple(vec![Norm::str("ok"), v]);
        let err = |c: &str, d: &str| Norm::tuple(vec![Norm::str("err"), Norm::str(c), Norm::str(d)]);
        let verb = args.first()?.as_str()?;
        let key = args.get(1).and_then(|k| k.as_str());
        Some(match (verb, key) {
            ("get", Some(k)) => ok(self.get(k).unwrap_or_else(Norm::nil)),
            ("put", Some(k)) => match args.get(2).map(|v| self.put(k, v)) {
                Some(Ok(())) => ok(Norm::nil()),
                Some(Err(e)) => err(e.code(), k),
                None => err("type", verb),
            },
            ("del", Some(k)) => match self.del(k) {
                Ok(()) => ok(Norm::nil()),
                Err(e) => err(e.code(), k),
            },
            ("list", Some(p)) => ok(Norm::list(self.list(p).iter().map(|k| Norm::str(k)).collect())),
            ("get" | "put" | "del" | "list", None) => err("type", verb),
            _ => err("verb", verb),
        })
    }
}

/// Where each origin's file lives: `<root>/<hex of BLAKE2b-256(site)>.gzs`.
pub fn path_for(root: &Path, site: &str) -> PathBuf {
    let h = k1ndl1ng_norm::hash::blake2b_256(site.as_bytes()).0;
    let name: String = h.iter().map(|b| format!("{b:02x}")).collect();
    root.join(format!("{name}.gzs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k1ndl1ng_norm::CollKind;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gaze-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("s.gzs")
    }

    #[test]
    fn put_get_list_del_persist() {
        let p = tmp("basic");
        {
            let mut s = OriginStore::open(&p, 1 << 20).unwrap();
            s.put("todo/1", &Norm::str("milk")).unwrap();
            s.put("todo/2", &Norm::list(vec![Norm::int(1), Norm::bool(true)])).unwrap();
            s.put("other", &Norm::nil()).unwrap();
            s.del("other").unwrap();
            assert_eq!(s.list("todo/"), vec!["todo/1", "todo/2"]);
        }
        let s = OriginStore::open(&p, 1 << 20).unwrap();
        assert_eq!(s.get("todo/1"), Some(Norm::str("milk")));
        assert!(s.get("other").is_none());
    }

    #[test]
    fn names_quota_and_torn_tails() {
        let p = tmp("rules");
        let mut s = OriginStore::open(&p, 64).unwrap();
        let name = Norm::eval(Name::Unforgeable([7; 32]));
        assert_eq!(s.put("k", &Norm::list(vec![name])), Err(StoreError::Unforgeable));
        assert_eq!(s.put("k", &Norm::str(&"x".repeat(100))), Err(StoreError::Quota));
        s.put("k", &Norm::int(1)).unwrap();
        drop(s);
        // A torn record at the end is dropped, not fatal.
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&[1, 5, b'a']).unwrap();
        drop(f);
        let mut s = OriginStore::open(&p, 64).unwrap();
        assert_eq!(s.get("k"), Some(Norm::int(1)));
        s.put("j", &Norm::int(2)).unwrap();
        drop(s);
        assert_eq!(OriginStore::open(&p, 64).unwrap().get("j"), Some(Norm::int(2)));
    }

    #[test]
    fn compaction_keeps_the_live_set() {
        let p = tmp("compact");
        let mut s = OriginStore::open(&p, 1 << 20).unwrap();
        for i in 0..5000 {
            s.put("counter", &Norm::int(i)).unwrap();
        }
        assert!(std::fs::metadata(&p).unwrap().len() < 80 * 1024);
        drop(s);
        assert_eq!(OriginStore::open(&p, 1 << 20).unwrap().get("counter"), Some(Norm::int(4999)));
    }

    #[test]
    fn protocol() {
        let p = tmp("proto");
        let mut s = OriginStore::open(&p, 1 << 20).unwrap();
        let r = s.serve(&[Norm::str("put"), Norm::str("a"), Norm::int(3)]).unwrap();
        assert_eq!(r, Norm::tuple(vec![Norm::str("ok"), Norm::nil()]));
        let r = s.serve(&[Norm::str("get"), Norm::str("a")]).unwrap();
        assert_eq!(r, Norm::tuple(vec![Norm::str("ok"), Norm::int(3)]));
        let r = s.serve(&[Norm::str("zap"), Norm::str("a")]).unwrap();
        assert_eq!(r.as_coll(CollKind::Tuple).unwrap()[1].as_str(), Some("verb"));
    }
}
