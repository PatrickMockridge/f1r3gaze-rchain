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

/// The phlo a blob publish is allowed to spend. Generous on purpose: a
/// `phlo_limit` is a *cap*, not a charge — the node bills the actual cost — so
/// the only thing a low limit buys is a failed deploy on a large site.
pub const BLOB_PHLO_LIMIT: i64 = 10_000_000;

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
pub fn manifest_for_dir(dir: &Path, entry: &str, mirrors: &[String]) -> Result<SiteManifest, String> {
    package_dir(dir, entry, mirrors).map(|(m, _)| m)
}

/// A packaged site: the manifest, and each file's bytes.
pub type Packed = (SiteManifest, Vec<(String, Vec<u8>)>);

/// The manifest **and** each file's bytes, so a caller that publishes the files
/// too does not read the tree twice — and so the bytes it publishes are the ones
/// the manifest's hashes were taken over.
pub fn package_dir(dir: &Path, entry: &str, mirrors: &[String]) -> Result<Packed, String> {
    let mut files = BTreeMap::new();
    let mut blobs = Vec::new();
    walk(dir, "", &mut files, &mut blobs)?;
    if !files.contains_key(entry) {
        return Err(format!("the entry {entry} is not in {}", dir.display()));
    }
    Ok((
        SiteManifest {
            entry: entry.to_string(),
            files,
            mirrors: mirrors.to_vec(),
        },
        blobs,
    ))
}

fn walk(
    dir: &Path,
    prefix: &str,
    out: &mut BTreeMap<String, [u8; 32]>,
    blobs: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), String> {
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
            walk(&p, &rel, out, blobs)?;
        } else {
            let b = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            out.insert(rel.clone(), gaze_net::digest(&b));
            blobs.push((rel, b));
        }
    }
    Ok(())
}

/// The on-chain blob root. The reader (`bridge::DriveSource`) is registered with
/// the same root, and reads under it with exploratory deploys that peek.
pub const DRIVE_ROOT: &str = "/gaze-blob/";

/// The deploy term that stores files on-chain in F1R3Drive's layout — the one
/// the existing reader already knows how to fetch, so a published site needs no
/// mirror at all.
///
/// Bytes are spelled `"<hex>".hexToBytes()`, and that is forced rather than
/// chosen: the CampF1R3 printer renders a byte array as `0x…`, but **rnode's
/// parser rejects that spelling** (`expected RParen, got Ident("x6869")`), so a
/// term built with `show` over `Norm::bytes` would not run here at all. A list
/// of integers parses but the reader's `bytes_of` does not accept it. `hexToBytes`
/// is the one spelling that both parses on this node and lands as a real
/// `ByteArray` — it is what the reference wallet already uses for a 65-byte key.
///
/// Files are capped at the reader's own limit; a larger file still needs a mirror.
pub fn blobs_term(files: &[([u8; 32], Vec<u8>)]) -> Result<String, String> {
    let mut sends = Vec::with_capacity(files.len());
    for (h, b) in files {
        if b.len() > crate::bridge::DRIVE_MAX {
            return Err(format!(
                "{} is {} bytes; the on-chain layout holds at most {} — serve it from a mirror instead",
                gaze_net::hex(h),
                b.len(),
                crate::bridge::DRIVE_MAX
            ));
        }
        sends.push(format!(
            "@\"{root}{h}\"!({{\"type\": \"f\", \"firstChunk\": \"{b}\".hexToBytes(), \"otherChunks\": {{}}}})",
            root = DRIVE_ROOT,
            h = gaze_net::hex(h),
            b = gaze_net::hex(b)
        ));
    }
    Ok(sends.join(" | "))
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

    #[test]
    fn the_blobs_term_writes_the_layout_the_reader_fetches() {
        let t = blobs_term(&[([0x22u8; 32], b"hi".to_vec())]).unwrap();
        let h = gaze_net::hex(&[0x22u8; 32]);
        // The reader peeks at exactly this name under this root.
        assert!(t.starts_with(&format!("@\"{DRIVE_ROOT}{h}\"!(")), "{t}");
        assert!(t.contains("\"type\": \"f\""), "{t}");
        // The one spelling that both parses on rnode and lands as a ByteArray.
        assert!(t.contains("\"firstChunk\": \"6869\".hexToBytes()"), "{t}");
        assert!(t.contains("\"otherChunks\": {}"), "{t}");
        // `0x…` is what the CampF1R3 printer emits for bytes, and rnode's
        // parser rejects it, so it must not appear.
        assert!(!t.contains("0x"), "{t}");

        // Several files ride one deploy, as parallel sends.
        let two = blobs_term(&[([1u8; 32], b"a".to_vec()), ([2u8; 32], b"b".to_vec())]).unwrap();
        assert_eq!(two.matches("!({").count(), 2, "{two}");

        // A file over the reader's limit is refused rather than truncated — a
        // truncated blob would still pass its own hash check.
        let big = vec![0u8; crate::bridge::DRIVE_MAX + 1];
        let e = blobs_term(&[([3u8; 32], big)]).unwrap_err();
        assert!(e.contains("at most"), "{e}");
    }

    #[test]
    fn package_dir_returns_the_bytes_its_hashes_were_taken_over() {
        let dir = std::env::temp_dir().join(format!("gaze-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), b"hi").unwrap();
        let (m, blobs) = package_dir(&dir, "index.html", &[]).unwrap();
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].0, "index.html");
        assert_eq!(blobs[0].1, b"hi");
        // The bytes are the ones the manifest's hash was taken over, which is
        // what lets a publisher upload them without re-reading the tree.
        assert_eq!(gaze_net::digest(&blobs[0].1), m.files["index.html"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
