//! Addresses and site manifests (spec §9.2–9.3).
//!
//! `f1r3://<publisher-hex>/<project>@<range>/<path>` resolves through the
//! registry entry `rho:serve:1:<publisher>:<project>:<range>`, whose data is
//! the site manifest: `{"gaze": 1, "entry": ..., "files": {name: hash},
//! "mirrors": [...]}`. `f1r3h://blake2b-256/<hex>` names content directly.

use k1ndl1ng_norm::{CollKind, Lit, Norm};
use std::collections::BTreeMap;

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
}
