//! `gaze-knf` — the `.knf` container (spec §5).
//!
//! ```text
//! magic    "F1R3KNF\0"
//! version  u16 LE, 1
//! level    u8: 0 K0, 1 K1, 2 K1G, 3 K2
//! sections (tag u8, len LEB128, bytes), tags ascending, each at most once
//!   0x01 manifest   a K1G map, encoded
//!   0x02 body       the ungrounded normal-form encoding
//!   0x03 source map optional
//! ```

#![forbid(unsafe_code)]

use k1ndl1ng_norm::hash::blake2b_256;
use k1ndl1ng_norm::term::leb;
use k1ndl1ng_norm::{normalise, CollKind, Norm, Options as NOpts};
use k1ndl1ng_parse::{parse, Level, Options as POpts};

pub const MAGIC: &[u8; 8] = b"F1R3KNF\0";
pub const VERSION: u16 = 1;
const SEC_MANIFEST: u8 = 0x01;
const SEC_BODY: u8 = 0x02;
const SEC_SRCMAP: u8 = 0x03;

/// The capability every conventional identifier requests when a page is
/// compiled from kernel text without an explicit manifest.
pub fn default_urn(ident: &str) -> Option<&'static str> {
    Some(match ident {
        "doc" => "rho:gaze:doc",
        "log" => "rho:gaze:log",
        "clock" => "rho:gaze:clock",
        "rand" => "rho:gaze:rand",
        "net" => "rho:gaze:net",
        "store" => "rho:gaze:store",
        "nav" => "rho:gaze:nav",
        "shard" => "rho:gaze:shard",
        _ => return None,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// Every free identifier of the body, in level order, with its capability.
    pub imports: Vec<(String, String)>,
    pub semiring: String,
    pub fair: bool,
    pub ceiling: String,
    pub frame_budget: u64,
    pub sync_budget: u64,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest {
            imports: Vec::new(),
            semiring: "boolean".into(),
            fair: false,
            ceiling: "crisp".into(),
            frame_budget: 20_000,
            sync_budget: 512,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnfError {
    BadMagic,
    BadVersion(u16),
    BadLevel(u8),
    Truncated,
    SectionOrder,
    MissingSection(&'static str),
    Decode(String),
    Manifest(&'static str),
    UnlistedImport(u32),
    Compile(Vec<String>),
    UnknownImport(String),
    Integrity,
}

impl Manifest {
    pub fn to_norm(&self) -> Norm {
        let imports = Norm::list(
            self.imports
                .iter()
                .map(|(i, u)| Norm::tuple(vec![Norm::str(i), Norm::str(u)]))
                .collect(),
        );
        let budget = Norm::map(vec![
            (Norm::str("frame"), Norm::int(self.frame_budget as i64)),
            (Norm::str("sync"), Norm::int(self.sync_budget as i64)),
        ]);
        Norm::map(vec![
            (Norm::str("imports"), imports),
            (Norm::str("semiring"), Norm::str(&self.semiring)),
            (Norm::str("fair"), Norm::bool(self.fair)),
            (Norm::str("ceiling"), Norm::str(&self.ceiling)),
            (Norm::str("budget"), budget),
        ])
    }

    pub fn from_norm(n: &Norm) -> Result<Manifest, KnfError> {
        let mut m = Manifest::default();
        let imports = n
            .map_get("imports")
            .and_then(|l| l.as_coll(CollKind::List))
            .ok_or(KnfError::Manifest("imports"))?;
        for t in imports {
            let pair = t.as_coll(CollKind::Tuple).ok_or(KnfError::Manifest("import"))?;
            match pair {
                [i, u] => m.imports.push((
                    i.as_str().ok_or(KnfError::Manifest("import ident"))?.to_string(),
                    u.as_str().ok_or(KnfError::Manifest("import urn"))?.to_string(),
                )),
                _ => return Err(KnfError::Manifest("import arity")),
            }
        }
        if let Some(s) = n.map_get("semiring") {
            m.semiring = s.as_str().ok_or(KnfError::Manifest("semiring"))?.to_string();
        }
        if let Some(f) = n.map_get("fair") {
            m.fair = f.as_bool().ok_or(KnfError::Manifest("fair"))?;
        }
        if let Some(c) = n.map_get("ceiling") {
            m.ceiling = c.as_str().ok_or(KnfError::Manifest("ceiling"))?.to_string();
        }
        if let Some(b) = n.map_get("budget") {
            if let Some(f) = b.map_get("frame").and_then(|x| x.as_int()) {
                m.frame_budget = f.max(1) as u64;
            }
            if let Some(s) = b.map_get("sync").and_then(|x| x.as_int()) {
                m.sync_budget = s.max(0) as u64;
            }
        }
        Ok(m)
    }
}

/// A page's behaviour, as shipped.
#[derive(Clone, Debug)]
pub struct Knf {
    pub level: Level,
    pub manifest: Manifest,
    /// Ungrounded: free names are levels, named by `manifest.imports`.
    pub body: Norm,
    pub source_map: Option<Vec<u8>>,
}

fn level_byte(l: Level) -> u8 {
    match l {
        Level::K0 => 0,
        Level::K1 => 1,
        Level::K1G => 2,
        Level::K2 => 3,
    }
}

fn level_of(b: u8) -> Result<Level, KnfError> {
    Ok(match b {
        0 => Level::K0,
        1 => Level::K1,
        2 => Level::K1G,
        3 => Level::K2,
        other => return Err(KnfError::BadLevel(other)),
    })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Knf {
    /// Compile kernel text. Imports come from `urns` where given, else from
    /// the conventional identifiers; any other free identifier is an error.
    pub fn from_source(src: &str, level: Level, urns: &[(&str, &str)]) -> Result<Knf, KnfError> {
        let p = parse(src, &POpts { level, ..POpts::default() });
        if !p.ok() {
            return Err(KnfError::Compile(
                p.diags.iter().map(|d| format!("{}: {}", d.code, d.message)).collect(),
            ));
        }
        let n = normalise(&p.tree, &NOpts::default()).map_err(|ds| {
            KnfError::Compile(ds.iter().map(|d| format!("{}: {}", d.code, d.message)).collect())
        })?;
        let mut imports = Vec::new();
        for f in &n.free {
            let urn = urns
                .iter()
                .find(|(i, _)| *i == f.ident)
                .map(|(_, u)| u.to_string())
                .or_else(|| default_urn(&f.ident).map(str::to_string))
                .ok_or_else(|| KnfError::UnknownImport(f.ident.clone()))?;
            imports.push((f.ident.clone(), urn));
        }
        Ok(Knf {
            level,
            manifest: Manifest {
                imports,
                ..Manifest::default()
            },
            body: n.term,
            source_map: None,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.push(level_byte(self.level));
        let section = |tag: u8, bytes: &[u8], out: &mut Vec<u8>| {
            out.push(tag);
            leb(bytes.len() as u32, out);
            out.extend_from_slice(bytes);
        };
        section(SEC_MANIFEST, self.manifest.to_norm().encode(), &mut out);
        section(SEC_BODY, self.body.encode(), &mut out);
        if let Some(sm) = &self.source_map {
            section(SEC_SRCMAP, sm, &mut out);
        }
        out
    }

    pub fn decode(b: &[u8]) -> Result<Knf, KnfError> {
        if b.len() < 11 || &b[..8] != MAGIC {
            return Err(KnfError::BadMagic);
        }
        let v = u16::from_le_bytes([b[8], b[9]]);
        if v != VERSION {
            return Err(KnfError::BadVersion(v));
        }
        let level = level_of(b[10])?;
        let mut i = 11;
        let mut last = 0u8;
        let (mut manifest, mut body, mut srcmap) = (None, None, None);
        while i < b.len() {
            let tag = b[i];
            i += 1;
            if tag <= last {
                return Err(KnfError::SectionOrder);
            }
            last = tag;
            let (len, used) = read_leb(&b[i..]).ok_or(KnfError::Truncated)?;
            i += used;
            let end = i.checked_add(len as usize).ok_or(KnfError::Truncated)?;
            let bytes = b.get(i..end).ok_or(KnfError::Truncated)?;
            i = end;
            match tag {
                SEC_MANIFEST => {
                    let n = Norm::decode(bytes).map_err(|e| KnfError::Decode(format!("{e:?}")))?;
                    manifest = Some(Manifest::from_norm(&n)?);
                }
                SEC_BODY => {
                    body = Some(Norm::decode(bytes).map_err(|e| KnfError::Decode(format!("{e:?}")))?)
                }
                SEC_SRCMAP => srcmap = Some(bytes.to_vec()),
                _ => {} // unknown sections are skipped: forward compatibility
            }
        }
        let manifest = manifest.ok_or(KnfError::MissingSection("manifest"))?;
        let body = body.ok_or(KnfError::MissingSection("body"))?;
        let k = Knf {
            level,
            manifest,
            body,
            source_map: srcmap,
        };
        k.check_imports()?;
        Ok(k)
    }

    /// Every free level of the body must be listed.
    pub fn check_imports(&self) -> Result<(), KnfError> {
        let n = self.manifest.imports.len() as u32;
        match free_levels(&self.body).into_iter().find(|l| *l >= n) {
            Some(l) => Err(KnfError::UnlistedImport(l)),
            None => Ok(()),
        }
    }

    /// The program hash: the executive's own hash of the body.
    pub fn program_hash(&self) -> [u8; 32] {
        self.body.hash().0
    }

    /// The grant hash: program hash and manifest together.
    pub fn grant_hash(&self) -> [u8; 32] {
        let mut pre = self.program_hash().to_vec();
        pre.extend_from_slice(self.manifest.to_norm().encode());
        blake2b_256(&pre).0
    }

    /// The HTML `integrity` attribute value.
    pub fn integrity(&self) -> String {
        format!("blake2b-256:{}", hex(&self.program_hash()))
    }

    /// Check an `integrity` attribute against this program.
    pub fn verify_integrity(&self, attr: &str) -> Result<(), KnfError> {
        if attr.trim().eq_ignore_ascii_case(&self.integrity()) {
            Ok(())
        } else {
            Err(KnfError::Integrity)
        }
    }
}

fn read_leb(b: &[u8]) -> Option<(u32, usize)> {
    let mut out = 0u32;
    for (i, c) in b.iter().enumerate().take(5) {
        out |= ((c & 0x7f) as u32) << (7 * i);
        if c & 0x80 == 0 {
            return Some((out, i + 1));
        }
    }
    None
}

/// The free levels a term mentions, as names or as process variables.
pub fn free_levels(t: &Norm) -> Vec<u32> {
    use k1ndl1ng_norm::{Name, Node};
    let mut out = std::collections::BTreeSet::new();
    let mut procs = vec![t.clone()];
    let mut names: Vec<Name> = Vec::new();
    loop {
        while let Some(n) = names.pop() {
            match n {
                Name::Free(l) => {
                    out.insert(l);
                }
                Name::Quote(q) => procs.push(q),
                _ => {}
            }
        }
        let Some(p) = procs.pop() else { break };
        match p.node() {
            Node::FreeVar(l) => {
                out.insert(*l);
            }
            Node::Par(v) => procs.extend(v.iter().cloned()),
            Node::Send { chan, args, .. } => {
                names.push(chan.clone());
                procs.extend(args.iter().cloned());
            }
            Node::Receive { binds, body } => {
                // Pattern variables are local; only channels are in scope.
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
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAMP: &str = r##"new found, sub, clicks, on, off in {
  doc!("query1", "#lamp", *found) |
  for (@("ok", *lamp) <- found) {
    lamp!("listen", "click", *clicks, {}, *sub) |
    off!(Nil) |
    for (_ <= clicks & _ <- off) { lamp!("classAdd", "lit")    | on!(Nil)  } |
    for (_ <= clicks & _ <- on)  { lamp!("classRemove", "lit") | off!(Nil) }
  }
}"##;

    #[test]
    fn round_trip_and_hashes() {
        let k = Knf::from_source(LAMP, Level::K1G, &[]).unwrap();
        assert_eq!(k.manifest.imports, vec![("doc".to_string(), "rho:gaze:doc".to_string())]);
        let bytes = k.encode();
        let back = Knf::decode(&bytes).unwrap();
        assert_eq!(back.program_hash(), k.program_hash());
        assert_eq!(back.grant_hash(), k.grant_hash());
        assert_eq!(back.manifest, k.manifest);
        back.verify_integrity(&k.integrity()).unwrap();
        assert!(back.verify_integrity("blake2b-256:00").is_err());
    }

    #[test]
    fn manifest_changes_grant_hash_not_program_hash() {
        let k = Knf::from_source(LAMP, Level::K1G, &[]).unwrap();
        let mut k2 = k.clone();
        k2.manifest.semiring = "viterbi".into();
        assert_eq!(k.program_hash(), k2.program_hash());
        assert_ne!(k.grant_hash(), k2.grant_hash());
    }

    #[test]
    fn unknown_identifiers_are_refused() {
        assert!(matches!(
            Knf::from_source("mystery!(1)", Level::K1G, &[]),
            Err(KnfError::UnknownImport(_))
        ));
        assert!(Knf::from_source("mystery!(1)", Level::K1G, &[("mystery", "rho:gaze:log")]).is_ok());
    }

    #[test]
    fn corrupt_containers_fail_cleanly() {
        let k = Knf::from_source(LAMP, Level::K1G, &[]).unwrap();
        let bytes = k.encode();
        for cut in [0, 5, 11, 20, bytes.len() - 1] {
            assert!(Knf::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert_eq!(Knf::decode(&bad).unwrap_err(), KnfError::BadMagic);
        // A manifest that lists too few imports is refused.
        let mut short = k.clone();
        short.manifest.imports.clear();
        assert!(matches!(Knf::decode(&short.encode()), Err(KnfError::UnlistedImport(0))));
    }
}
