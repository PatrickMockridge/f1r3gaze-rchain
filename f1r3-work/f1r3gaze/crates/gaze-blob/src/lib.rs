//! `gaze-blob` — content by hash (spec §9.7).
//!
//! Every source is untrusted; the hash decides. Sources are tried in order:
//! the local [`ContentCache`], then mirrors (the site manifest's and the
//! user's, `GET <mirror><hex>`), then any further sources the shell adds —
//! the shard bridge adds the on-chain F1R3Drive layout for small blobs.
//! Whatever verifies is written to the cache.

#![forbid(unsafe_code)]

use gaze_net::{Http, HttpRequest, digest, hex};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub type Hash = [u8; 32];

/// Somewhere a blob might be. `Ok(None)` means "not here".
pub trait BlobSource: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn get(&self, h: &Hash) -> Result<Option<Vec<u8>>, String>;
}

/// `<dir>/<aa>/<hex>`: verified on every read, evicted oldest-first past
/// `max_bytes`.
pub struct ContentCache {
    dir: PathBuf,
    max_bytes: u64,
}

impl ContentCache {
    pub fn new(dir: impl AsRef<Path>, max_bytes: u64) -> ContentCache {
        ContentCache {
            dir: dir.as_ref().to_path_buf(),
            max_bytes,
        }
    }

    fn path(&self, h: &Hash) -> PathBuf {
        let x = hex(h);
        self.dir.join(&x[..2]).join(x)
    }

    pub fn get(&self, h: &Hash) -> Option<Vec<u8>> {
        let p = self.path(h);
        let b = std::fs::read(&p).ok()?;
        if digest(&b) == *h {
            Some(b)
        } else {
            let _ = std::fs::remove_file(p); // corrupt on disk
            None
        }
    }

    pub fn put(&self, h: &Hash, b: &[u8]) -> Result<(), String> {
        if digest(b) != *h {
            return Err("hash mismatch".into());
        }
        let p = self.path(h);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let tmp = p.with_extension("part");
        std::fs::write(&tmp, b).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &p).map_err(|e| e.to_string())?;
        self.evict();
        Ok(())
    }

    /// Drop the oldest files until the cache fits.
    pub fn evict(&self) {
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        let Ok(top) = std::fs::read_dir(&self.dir) else { return };
        for sub in top.flatten() {
            let Ok(rd) = std::fs::read_dir(sub.path()) else { continue };
            for f in rd.flatten() {
                if let Ok(m) = f.metadata() {
                    files.push((m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), f.path()));
                }
            }
        }
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        if total <= self.max_bytes {
            return;
        }
        files.sort();
        for (_, len, p) in files {
            if total <= self.max_bytes {
                break;
            }
            if std::fs::remove_file(&p).is_ok() {
                total -= len;
            }
        }
    }
}

/// `GET <base><hex>`.
pub struct MirrorSource {
    base: String,
    http: Http,
}

impl MirrorSource {
    pub fn new(base: &str, http: Http) -> MirrorSource {
        let base = if base.ends_with('/') { base.to_string() } else { format!("{base}/") };
        MirrorSource { base, http }
    }
}

impl BlobSource for MirrorSource {
    fn name(&self) -> &str {
        &self.base
    }
    fn get(&self, h: &Hash) -> Result<Option<Vec<u8>>, String> {
        let url = format!("{}{}", self.base, hex(h));
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(format!("mirror is not http(s): {url}"));
        }
        let r = self
            .http
            .send_following(
                &HttpRequest {
                    url,
                    method: "GET".into(),
                    ..Default::default()
                },
                &|_| true,
            )
            .map_err(|e| e.to_string())?;
        match r.status {
            200..=299 => Ok(Some(r.body)),
            404 | 410 => Ok(None),
            s => Err(format!("mirror {}: HTTP {s}", self.base)),
        }
    }
}

/// The blob resolver the shell shares between tabs.
pub struct Blobs {
    pub cache: ContentCache,
    http: Http,
    sources: RwLock<Vec<Arc<dyn BlobSource>>>,
}

impl Blobs {
    pub fn new(cache: ContentCache, http: Http) -> Blobs {
        Blobs {
            cache,
            http,
            sources: RwLock::new(Vec::new()),
        }
    }

    pub fn add_source(&self, s: Arc<dyn BlobSource>) {
        if let Ok(mut v) = self.sources.write() {
            v.push(s);
        }
    }

    /// Find a blob: cache, then `mirrors` (e.g. a site manifest's), then the
    /// configured sources. Every answer is checked against `h`.
    pub fn get(&self, h: &Hash, mirrors: &[String]) -> Result<Vec<u8>, String> {
        if let Some(b) = self.cache.get(h) {
            return Ok(b);
        }
        let mut tried = Vec::new();
        let mut chain: Vec<Arc<dyn BlobSource>> =
            mirrors.iter().map(|m| Arc::new(MirrorSource::new(m, self.http.clone())) as Arc<dyn BlobSource>).collect();
        if let Ok(v) = self.sources.read() {
            chain.extend(v.iter().cloned());
        }
        for s in chain {
            match s.get(h) {
                Ok(Some(b)) if digest(&b) == *h => {
                    let _ = self.cache.put(h, &b);
                    return Ok(b);
                }
                Ok(Some(_)) => tried.push(format!("{}: wrong content", s.name())),
                Ok(None) => tried.push(format!("{}: absent", s.name())),
                Err(e) => tried.push(format!("{}: {e}", s.name())),
            }
        }
        Err(format!("blob {} not found ({})", hex(h), tried.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Liar;
    impl BlobSource for Liar {
        fn name(&self) -> &str {
            "liar"
        }
        fn get(&self, _h: &Hash) -> Result<Option<Vec<u8>>, String> {
            Ok(Some(b"not it".to_vec()))
        }
    }
    struct Honest(Vec<u8>);
    impl BlobSource for Honest {
        fn name(&self) -> &str {
            "honest"
        }
        fn get(&self, h: &Hash) -> Result<Option<Vec<u8>>, String> {
            Ok(if digest(&self.0) == *h { Some(self.0.clone()) } else { None })
        }
    }

    #[test]
    fn the_hash_decides_and_the_cache_remembers() {
        let dir = std::env::temp_dir().join(format!("gaze-blob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let blobs = Blobs::new(ContentCache::new(&dir, 1 << 20), Http::new());
        blobs.add_source(Arc::new(Liar));
        let h = digest(b"page");
        assert!(blobs.get(&h, &[]).unwrap_err().contains("liar: wrong content"));
        blobs.add_source(Arc::new(Honest(b"page".to_vec())));
        assert_eq!(blobs.get(&h, &[]).unwrap(), b"page");
        // Now from the cache, even with only liars around.
        let again = Blobs::new(ContentCache::new(&dir, 1 << 20), Http::new());
        again.add_source(Arc::new(Liar));
        assert_eq!(again.get(&h, &[]).unwrap(), b"page");
        // A corrupted cache file is detected and dropped.
        std::fs::write(again.cache.path(&h), b"rot").unwrap();
        assert!(again.cache.get(&h).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eviction_bounds_the_cache() {
        let dir = std::env::temp_dir().join(format!("gaze-blob-evict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let c = ContentCache::new(&dir, 2500);
        for i in 0..10u8 {
            let b = vec![i; 1000];
            c.put(&digest(&b), &b).unwrap();
        }
        let n: usize = std::fs::read_dir(&dir).unwrap().flatten().map(|d| std::fs::read_dir(d.path()).unwrap().count()).sum();
        assert!(n <= 2, "{n}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
