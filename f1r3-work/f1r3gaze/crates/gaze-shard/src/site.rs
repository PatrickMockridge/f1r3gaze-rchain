//! Addresses and site manifests (spec §9.2–9.3).
//!
//! `f1r3://<publisher-hex>/<project>@<range>/<path>` resolves through the
//! registry entry `rho:serve:1:<publisher>:<project>:<range>`, whose data is
//! the site manifest: `{"gaze": 1, "entry": ..., "files": {name: hash},
//! "mirrors": [...]}`. `f1r3h://blake2b-256/<hex>` names content directly.

use k1ndl1ng_norm::{CollKind, Lit, Norm};
use std::collections::BTreeMap;
use std::path::Path;

/// The phlo a site publish is allowed to spend: one send carrying the whole
/// manifest, so it scales with the file count rather than with a handful of
/// reduction steps.
pub const PUBLISH_PHLO_LIMIT: i64 = 1_000_000;

/// Escape a channel name for a rholang string literal. A project or range comes
/// from the address and may hold anything, so it is escaped rather than trusted.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The manifest as it goes on the wire: identical in shape to
/// [`SiteManifest::to_norm`] **except** that a file hash is a hex *string*
/// rather than a byte array.
///
/// That is not cosmetic. `show` renders a byte array as a `0x…` literal, and
/// whether the node's rholang parser takes that spelling is not something this
/// client should assume; the hex-string spelling is what `f1r3c site` already
/// emits and what [`SiteManifest::from_norm`] documents as accepted. The
/// escaping is still the printer's, so a file name containing a quote cannot
/// break out of its literal.
fn manifest_norm(m: &SiteManifest) -> Norm {
    Norm::map(vec![
        (Norm::str("gaze"), Norm::int(1)),
        (Norm::str("entry"), Norm::str(&m.entry)),
        (
            Norm::str("files"),
            Norm::map(m.files.iter().map(|(k, h)| (Norm::str(k), Norm::str(&gaze_net::hex(h)))).collect()),
        ),
        (Norm::str("mirrors"), Norm::list(m.mirrors.iter().map(|x| Norm::str(x)).collect())),
    ])
}

/// The deploy term that publishes a manifest: one send of it to the public
/// channel the site's address names.
///
/// On rchain a site *is* data at a public name — there is no `/api/registry`,
/// and `rchain::registry` reads whatever this puts there under the same
/// `rho:serve:1:…` string f1r3fly uses as a registry URI. So a site's `f1r3://`
/// address is portable between the two dialects, but a manifest published on
/// f1r3fly has to be **re-published** here: the writer differs even though the
/// address does not.
pub fn publish_term(uri: &str, m: &SiteManifest) -> String {
    format!("@\"{}\"!({})", esc(uri), k1ndl1ng_norm::show(&manifest_norm(m)))
}

/// The manifest for a directory: every file hashed, dotfiles skipped (as
/// `f1r3c site` does), and the entry required to be one of them.
///
/// The *files* are not published here — only their hashes and, through the
/// manifest, the mirrors that carry them.
pub fn manifest_for_dir(dir: &Path, entry: &str, mirrors: &[String]) -> Result<SiteManifest, String> {
    let mut files = BTreeMap::new();
    walk(dir, "", &mut files)?;
    if !files.contains_key(entry) {
        return Err(format!("the entry {entry} is not in {}", dir.display()));
    }
    Ok(SiteManifest {
        entry: entry.to_string(),
        files,
        mirrors: mirrors.to_vec(),
    })
}

fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, [u8; 32]>) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for e in rd {
        let e = e.map_err(|e| e.to_string())?;
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let p = e.path();
        if e.file_type().map_err(|e| e.to_string())?.is_dir() {
            walk(&p, &rel, out)?;
        } else {
            let b = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            out.insert(rel, gaze_net::digest(&b));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteAddr {
    pub publisher: String,
    pub project: String,
    pub range: String,
    pub path: String,
}

impl SiteAddr {
    pub fn parse(url: &str) -> Option<SiteAddr> {
        let rest = url.strip_prefix("f1r3://")?;
        let rest = rest.split(['?', '#']).next()?;
        let (publisher, rest) = rest.split_once('/')?;
        let (proj, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (project, range) = proj.split_once('@').unwrap_or((proj, "*"));
        if publisher.is_empty() || !publisher.chars().all(|c| c.is_ascii_hexdigit()) || project.is_empty() {
            return None;
        }
        Some(SiteAddr {
            publisher: publisher.to_ascii_lowercase(),
            project: project.into(),
            range: range.into(),
            path: path.into(),
        })
    }
    pub fn registry_uri(&self) -> String {
        format!("rho:serve:1:{}:{}:{}", self.publisher, self.project, self.range)
    }
    /// The freshness key: one binding per publisher and project.
    pub fn binding(&self) -> String {
        format!("{}/{}", self.publisher, self.project)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteManifest {
    pub entry: String,
    pub files: BTreeMap<String, [u8; 32]>,
    pub mirrors: Vec<String>,
}

impl SiteManifest {
    pub fn from_norm(n: &Norm) -> Result<SiteManifest, String> {
        if n.map_get("gaze").and_then(|g| g.as_int()) != Some(1) {
            return Err("not a gaze site manifest (version 1)".into());
        }
        let entry = n.map_get("entry").and_then(|e| e.as_str()).unwrap_or("index.html").to_string();
        let mut files = BTreeMap::new();
        for kv in n.map_get("files").and_then(|f| f.as_coll(CollKind::Map)).ok_or("manifest has no files")?.chunks(2) {
            let name = kv[0].as_str().ok_or("file name")?.to_string();
            let h: [u8; 32] = match kv[1].as_lit() {
                Some(Lit::Bytes(b)) => b.to_vec().try_into().map_err(|_| "file hash is not 32 bytes")?,
                _ => match kv[1].as_str().and_then(gaze_net::unhex) {
                    Some(b) => b.try_into().map_err(|_| "file hash is not 32 bytes")?,
                    None => return Err(format!("bad hash for {name}")),
                },
            };
            files.insert(name, h);
        }
        let mirrors = n
            .map_get("mirrors")
            .and_then(|m| m.as_coll(CollKind::List))
            .map(|l| l.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        Ok(SiteManifest { entry, files, mirrors })
    }

    pub fn to_norm(&self) -> Norm {
        Norm::map(vec![
            (Norm::str("gaze"), Norm::int(1)),
            (Norm::str("entry"), Norm::str(&self.entry)),
            (
                Norm::str("files"),
                Norm::map(self.files.iter().map(|(k, h)| (Norm::str(k), Norm::bytes(h))).collect()),
            ),
            (Norm::str("mirrors"), Norm::list(self.mirrors.iter().map(|m| Norm::str(m)).collect())),
        ])
    }

    /// The file a path names; "" and directories map to the entry.
    pub fn file_for(&self, path: &str) -> Option<(&str, [u8; 32])> {
        let p = path.trim_start_matches('/');
        let p = if p.is_empty() || p.ends_with('/') { self.entry.as_str() } else { p };
        self.files.get_key_value(p).map(|(k, h)| (k.as_str(), *h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse() {
        let a = SiteAddr::parse("f1r3://ABCD01/todo@^1.2/app/index.html?x").unwrap();
        assert_eq!(a.publisher, "abcd01");
        assert_eq!(a.registry_uri(), "rho:serve:1:abcd01:todo:^1.2");
        assert_eq!(a.path, "app/index.html");
        assert_eq!(SiteAddr::parse("f1r3://ab/todo").unwrap().range, "*");
        assert!(SiteAddr::parse("f1r3://zz/todo").is_none());
    }

    #[test]
    fn manifests_round_trip() {
        let mut files = BTreeMap::new();
        files.insert("index.html".to_string(), [1u8; 32]);
        let m = SiteManifest {
            entry: "index.html".into(),
            files,
            mirrors: vec!["https://m/".into()],
        };
        assert_eq!(SiteManifest::from_norm(&m.to_norm()).unwrap(), m);
        assert_eq!(m.file_for("/").unwrap().0, "index.html");
        assert!(m.file_for("missing").is_none());
    }

    #[test]
    fn the_publish_term_names_the_channel_and_carries_the_manifest() {
        let mut files = BTreeMap::new();
        files.insert("index.html".to_string(), [0x11u8; 32]);
        let m = SiteManifest {
            entry: "index.html".into(),
            files,
            mirrors: vec!["https://m/".into()],
        };
        let t = publish_term("rho:serve:1:abcd:todo:^1", &m);
        assert!(t.starts_with("@\"rho:serve:1:abcd:todo:^1\"!("), "{t}");
        assert!(t.contains("\"gaze\": 1"), "{t}");
        assert!(t.contains("\"entry\": \"index.html\""), "{t}");
        assert!(t.contains(&format!("\"index.html\": \"{}\"", gaze_net::hex(&[0x11u8; 32]))), "{t}");
        assert!(t.contains("\"mirrors\": [\"https://m/\"]"), "{t}");
        // A hash goes out as a hex string, never a byte literal: the node's
        // parser is not assumed to take the `0x…` spelling.
        assert!(!t.contains("0x"), "{t}");

        // The content round-trips through the reader's own parser.
        assert_eq!(SiteManifest::from_norm(&manifest_norm(&m)).unwrap(), m);

        // A file name that holds a quote cannot break out of its literal.
        let mut odd = BTreeMap::new();
        odd.insert("a\"b.html".to_string(), [2u8; 32]);
        let om = SiteManifest { entry: "a\"b.html".into(), files: odd, mirrors: vec![] };
        let ot = publish_term("rho:serve:1:ab:p:^1", &om);
        assert!(ot.contains("\\\""), "the quote is escaped: {ot}");
        assert_eq!(SiteManifest::from_norm(&manifest_norm(&om)).unwrap(), om);
    }

    #[test]
    fn a_directory_becomes_a_manifest() {
        let dir = std::env::temp_dir().join(format!("gaze-site-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("index.html"), b"<h1>hi</h1>").unwrap();
        std::fs::write(dir.join("sub/app.js"), b"1").unwrap();
        std::fs::write(dir.join(".hidden"), b"no").unwrap();

        let m = manifest_for_dir(&dir, "index.html", &["https://m/".into()]).unwrap();
        assert_eq!(m.files.len(), 2, "the dotfile is skipped");
        assert_eq!(m.files["index.html"], gaze_net::digest(b"<h1>hi</h1>"));
        assert!(m.files.contains_key("sub/app.js"), "nested paths keep their name");
        assert_eq!(m.entry, "index.html");
        // And it survives the trip to the node and back.
        assert_eq!(SiteManifest::from_norm(&manifest_norm(&m)).unwrap(), m);

        // An entry that is not there is refused before anything is published.
        assert!(manifest_for_dir(&dir, "missing.html", &[]).unwrap_err().contains("not in"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
