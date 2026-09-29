//! `gaze-broker` — the capability broker (spec §8).
//!
//! The broker turns a page's manifest into grants. A grant is a routing entry
//! `(tab, capability) ↦ (attenuation, quota)`; the page only ever holds the
//! aperture's unforgeable name, which the tab executive binds. Revocation is
//! a table update: the broker drops the entry and tells the shell, which calls
//! `TabExec::revoke`, after which every send on the name answers
//! `("err", "revoked")`.
//!
//! Grants are remembered per *(site identity, grant hash)*: the manifest, not
//! the body, states the authority requested, so a new manifest is a new
//! request. Prompts are not answered synchronously — the broker returns a
//! [`GrantPlan`] whose undecided entries the shell shows to the user before
//! anything runs, and [`Broker::answer`] records the choice.
//!
//! Portable: std only, no unsafe, no clocks.

#![forbid(unsafe_code)]

use gaze_knf::Knf;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

pub const DOC: &str = "rho:gaze:doc";
pub const LOG: &str = "rho:gaze:log";
pub const CLOCK: &str = "rho:gaze:clock";
pub const RAND: &str = "rho:gaze:rand";
pub const NET: &str = "rho:gaze:net";
pub const STORE: &str = "rho:gaze:store";
pub const NAV: &str = "rho:gaze:nav";
pub const SHARD: &str = "rho:gaze:shard";
pub const DEBUG: &str = "rho:gaze:debug";

/// Who is asking: an origin (`https://example.org:443`) or a shard site
/// (`f1r3://<publisher-hex>/<project>`). Built only through the constructors,
/// so two spellings of one origin compare equal.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Site(String);

impl Site {
    /// The site of a URL: scheme, host and effective port for web URLs;
    /// publisher and project for `f1r3://`; the whole hash for `f1r3h://`;
    /// `file://` pages are one site each (by path), never shared.
    pub fn of_url(url: &str) -> Option<Site> {
        let (scheme, rest) = url.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        match scheme.as_str() {
            "http" | "https" => {
                let auth = rest.split(['/', '?', '#']).next()?;
                let auth = auth.rsplit('@').next()?.to_ascii_lowercase();
                if auth.is_empty() {
                    return None;
                }
                let (host, port) = match auth.rsplit_once(':') {
                    Some((h, p)) if !h.ends_with(']') || auth.starts_with('[') && h.ends_with(']') => {
                        if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() {
                            (h.to_string(), p.to_string())
                        } else {
                            (auth.clone(), default_port(&scheme).into())
                        }
                    }
                    _ => (auth.clone(), default_port(&scheme).into()),
                };
                Some(Site(format!("{scheme}://{host}:{port}")))
            }
            "f1r3" => {
                let mut parts = rest.split('/');
                let publisher = parts.next()?.to_ascii_lowercase();
                let project = parts.next()?.split('@').next()?.to_string();
                if publisher.is_empty() || project.is_empty() {
                    return None;
                }
                Some(Site(format!("f1r3://{publisher}/{project}")))
            }
            "f1r3h" => Some(Site(format!("f1r3h://{}", rest.split(['?', '#']).next()?.to_ascii_lowercase()))),
            "file" => Some(Site(format!("file://{}", rest.split(['?', '#']).next()?))),
            "gaze" => Some(Site(format!("gaze://{}", rest.split(['/', '?', '#']).next()?))),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Built-in pages (`gaze://…`) are the browser's own.
    pub fn is_internal(&self) -> bool {
        self.0.starts_with("gaze://")
    }

    /// Same site for navigation and `net`: same scheme, host and port. For
    /// shard sites, same publisher and project.
    pub fn same_site(&self, other: &Site) -> bool {
        self == other
    }
}

fn default_port(scheme: &str) -> &'static str {
    match scheme {
        "https" => "443",
        _ => "80",
    }
}

impl fmt::Display for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the policy says before the user is asked anything.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Default_ {
    Grant,
    Prompt,
    Deny,
}

/// The default policy table of spec §8.1.
pub fn default_for(urn: &str, site: &Site) -> Default_ {
    match urn {
        DOC | LOG | CLOCK | RAND | NET | STORE | NAV => Default_::Grant,
        SHARD => Default_::Prompt,
        DEBUG if site.is_internal() => Default_::Grant,
        _ => Default_::Deny,
    }
}

/// The shard bridge's verb classes (spec §8.1: "per verb class").
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ShardClass {
    Read,
    Explore,
    Deploy,
    Session,
}

impl ShardClass {
    pub const ALL: [ShardClass; 4] = [ShardClass::Read, ShardClass::Explore, ShardClass::Deploy, ShardClass::Session];
    pub fn name(self) -> &'static str {
        match self {
            ShardClass::Read => "read",
            ShardClass::Explore => "explore",
            ShardClass::Deploy => "deploy",
            ShardClass::Session => "session",
        }
    }
    pub fn parse(s: &str) -> Option<ShardClass> {
        ShardClass::ALL.into_iter().find(|c| c.name() == s)
    }
    /// The class a page verb belongs to.
    pub fn of_verb(verb: &str) -> Option<ShardClass> {
        Some(match verb {
            "lookup" | "read" | "watch" => ShardClass::Read,
            "explore" => ShardClass::Explore,
            "deploy" => ShardClass::Deploy,
            "session" => ShardClass::Session,
            _ => return None,
        })
    }
}

/// How a granted capability is narrowed.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Attenuation {
    /// `net`: sites the page may fetch from. The page's own site is always
    /// included; others only by prompt.
    pub net_sites: BTreeSet<Site>,
    /// `shard`: verb classes allowed.
    pub shard: BTreeSet<ShardClass>,
    /// `store`: bytes the origin may keep.
    pub store_quota: u64,
}

pub const DEFAULT_STORE_QUOTA: u64 = 10 * 1024 * 1024;

/// One capability's status in a plan.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Status {
    Granted,
    Denied,
    /// The user must be asked; the text says what for.
    Ask(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlanEntry {
    pub ident: String,
    pub urn: String,
    pub status: Status,
}

/// The broker's answer for one page load.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GrantPlan {
    pub site: Site,
    pub grant_hash: [u8; 32],
    pub entries: Vec<PlanEntry>,
}

impl GrantPlan {
    pub fn is_settled(&self) -> bool {
        !self.entries.iter().any(|e| matches!(e.status, Status::Ask(_)))
    }
    /// The executive's policy function, once settled: anything still
    /// undecided is denied.
    pub fn granted(&self, urn: &str) -> bool {
        self.entries.iter().any(|e| e.urn == urn && e.status == Status::Granted)
    }
    pub fn pending(&self) -> impl Iterator<Item = &PlanEntry> {
        self.entries.iter().filter(|e| matches!(e.status, Status::Ask(_)))
    }
}

/// A remembered decision.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Stored {
    pub site: Site,
    pub grant_hash: [u8; 32],
    pub urn: String,
    pub allow: bool,
    /// For `shard`: the classes allowed. Empty for other capabilities.
    pub classes: BTreeSet<ShardClass>,
}

/// Where decisions persist.
pub trait GrantStore {
    fn load(&self) -> Vec<Stored>;
    fn save(&mut self, all: &[Stored]) -> Result<(), String>;
}

/// In memory; for tests and private windows.
#[derive(Default)]
pub struct MemGrants(pub Vec<Stored>);
impl GrantStore for MemGrants {
    fn load(&self) -> Vec<Stored> {
        self.0.clone()
    }
    fn save(&mut self, all: &[Stored]) -> Result<(), String> {
        self.0 = all.to_vec();
        Ok(())
    }
}

/// One line per decision, tab-separated:
/// `site  grant-hash-hex  urn  allow|deny  class,class`. Written atomically
/// (temporary file, then rename).
pub struct FileGrants {
    path: PathBuf,
}

impl FileGrants {
    pub fn new(path: impl AsRef<Path>) -> FileGrants {
        FileGrants {
            path: path.as_ref().to_path_buf(),
        }
    }
}

fn hex32(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

impl GrantStore for FileGrants {
    fn load(&self) -> Vec<Stored> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return Vec::new() };
        text.lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                if f.len() != 5 {
                    return None;
                }
                Some(Stored {
                    site: Site(f[0].to_string()),
                    grant_hash: unhex32(f[1])?,
                    urn: f[2].to_string(),
                    allow: f[3] == "allow",
                    classes: f[4].split(',').filter_map(ShardClass::parse).collect(),
                })
            })
            .collect()
    }
    fn save(&mut self, all: &[Stored]) -> Result<(), String> {
        let mut out = String::new();
        for s in all {
            let classes: Vec<&str> = s.classes.iter().map(|c| c.name()).collect();
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                s.site,
                hex32(&s.grant_hash),
                s.urn,
                if s.allow { "allow" } else { "deny" },
                classes.join(",")
            ));
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }
}

pub type TabId = u64;

/// A live routing entry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Route {
    pub site: Site,
    pub urn: String,
    pub att: Attenuation,
}

/// Why a request through a live route was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refusal {
    Revoked,
    /// Needs the user; the text is the prompt.
    Ask(String),
    Denied(String),
}

pub struct Broker<S: GrantStore> {
    store: S,
    stored: Vec<Stored>,
    routes: BTreeMap<(TabId, String), Route>,
}

impl<S: GrantStore> Broker<S> {
    pub fn new(store: S) -> Broker<S> {
        let stored = store.load();
        Broker {
            store,
            stored,
            routes: BTreeMap::new(),
        }
    }

    fn remembered(&self, site: &Site, gh: &[u8; 32], urn: &str) -> Option<&Stored> {
        self.stored.iter().find(|s| &s.site == site && &s.grant_hash == gh && s.urn == urn)
    }

    /// Turn a page's manifest into a plan.
    pub fn plan(&self, site: &Site, knf: &Knf) -> GrantPlan {
        let gh = knf.grant_hash();
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        for (ident, urn) in &knf.manifest.imports {
            let status = if !seen.insert(urn.clone()) {
                // Same capability under a second identifier: same decision.
                entries
                    .iter()
                    .find(|e: &&PlanEntry| &e.urn == urn)
                    .map(|e| e.status.clone())
                    .unwrap_or(Status::Denied)
            } else if let Some(s) = self.remembered(site, &gh, urn) {
                if s.allow { Status::Granted } else { Status::Denied }
            } else {
                match default_for(urn, site) {
                    Default_::Grant => Status::Granted,
                    Default_::Deny => Status::Denied,
                    Default_::Prompt => Status::Ask(prompt_text(site, urn)),
                }
            };
            entries.push(PlanEntry {
                ident: ident.clone(),
                urn: urn.clone(),
                status,
            });
        }
        GrantPlan {
            site: site.clone(),
            grant_hash: gh,
            entries,
        }
    }

    /// Record the user's answer for one pending capability. With `remember`,
    /// it persists for this (site, grant hash).
    pub fn answer(&mut self, plan: &mut GrantPlan, urn: &str, allow: bool, remember: bool) -> Result<(), String> {
        for e in plan.entries.iter_mut().filter(|e| e.urn == urn) {
            e.status = if allow { Status::Granted } else { Status::Denied };
        }
        if remember {
            self.stored
                .retain(|s| !(s.site == plan.site && s.grant_hash == plan.grant_hash && s.urn == urn));
            let classes = if urn == SHARD && allow {
                [ShardClass::Read, ShardClass::Explore].into_iter().collect()
            } else {
                BTreeSet::new()
            };
            self.stored.push(Stored {
                site: plan.site.clone(),
                grant_hash: plan.grant_hash,
                urn: urn.to_string(),
                allow,
                classes,
            });
            self.store.save(&self.stored)?;
        }
        Ok(())
    }

    /// Install a settled plan's grants as routes for `tab`.
    pub fn install(&mut self, tab: TabId, plan: &GrantPlan) {
        for e in plan.entries.iter().filter(|e| e.status == Status::Granted) {
            let mut att = Attenuation {
                store_quota: DEFAULT_STORE_QUOTA,
                ..Default::default()
            };
            att.net_sites.insert(plan.site.clone());
            if e.urn == SHARD {
                // Granting `shard` grants reading and exploring; deploys and
                // sessions are asked for per request, unless remembered.
                att.shard.insert(ShardClass::Read);
                att.shard.insert(ShardClass::Explore);
                if let Some(s) = self.remembered(&plan.site, &plan.grant_hash, SHARD) {
                    att.shard.extend(s.classes.iter().copied());
                }
            }
            self.routes.insert(
                (tab, e.urn.clone()),
                Route {
                    site: plan.site.clone(),
                    urn: e.urn.clone(),
                    att,
                },
            );
        }
    }

    pub fn route(&self, tab: TabId, urn: &str) -> Option<&Route> {
        self.routes.get(&(tab, urn.to_string()))
    }

    /// Check a `net` fetch against the route. Other sites need a prompt.
    pub fn check_net(&self, tab: TabId, url: &str) -> Result<(), Refusal> {
        let r = self.route(tab, NET).ok_or(Refusal::Revoked)?;
        let target = Site::of_url(url).ok_or_else(|| Refusal::Denied(format!("not a fetchable URL: {url}")))?;
        if !matches!(target.as_str().split("://").next(), Some("http" | "https" | "f1r3h" | "f1r3")) {
            return Err(Refusal::Denied(format!("scheme not fetchable: {url}")));
        }
        if r.att.net_sites.contains(&target) {
            Ok(())
        } else {
            Err(Refusal::Ask(format!("{} wants to fetch from {}", r.site, target)))
        }
    }

    /// Widen a tab's `net` to another site (the user said yes).
    pub fn allow_net(&mut self, tab: TabId, site: Site) {
        if let Some(r) = self.routes.get_mut(&(tab, NET.to_string())) {
            r.att.net_sites.insert(site);
        }
    }

    /// Check a shard verb against the route.
    pub fn check_shard(&self, tab: TabId, verb: &str) -> Result<ShardClass, Refusal> {
        let r = self.route(tab, SHARD).ok_or(Refusal::Revoked)?;
        let class = ShardClass::of_verb(verb).ok_or_else(|| Refusal::Denied(format!("unknown shard verb {verb}")))?;
        if r.att.shard.contains(&class) {
            Ok(class)
        } else {
            Err(Refusal::Ask(format!("{} wants to {} on the shard", r.site, class.name())))
        }
    }

    /// Allow a shard verb class for this tab; optionally remember it.
    pub fn allow_shard(&mut self, tab: TabId, class: ShardClass, remember: Option<[u8; 32]>) -> Result<(), String> {
        let site = match self.routes.get_mut(&(tab, SHARD.to_string())) {
            Some(r) => {
                r.att.shard.insert(class);
                r.site.clone()
            }
            None => return Ok(()),
        };
        if let Some(gh) = remember {
            if let Some(s) = self
                .stored
                .iter_mut()
                .find(|s| s.site == site && s.grant_hash == gh && s.urn == SHARD)
            {
                s.classes.insert(class);
            } else {
                self.stored.push(Stored {
                    site,
                    grant_hash: gh,
                    urn: SHARD.into(),
                    allow: true,
                    classes: [ShardClass::Read, ShardClass::Explore, class].into_iter().collect(),
                });
            }
            self.store.save(&self.stored)?;
        }
        Ok(())
    }

    /// Revoke one capability of a tab. Returns true if a route was removed;
    /// the caller then calls `TabExec::revoke(urn)`.
    pub fn revoke(&mut self, tab: TabId, urn: &str) -> bool {
        self.routes.remove(&(tab, urn.to_string())).is_some()
    }

    /// Forget a remembered decision (settings page).
    pub fn forget(&mut self, site: &Site) -> Result<(), String> {
        self.stored.retain(|s| &s.site != site);
        self.store.save(&self.stored)
    }

    /// Drop every route of a closed tab.
    pub fn close_tab(&mut self, tab: TabId) {
        self.routes.retain(|(t, _), _| *t != tab);
    }

    /// A tab's live grants, for the shell's panel.
    pub fn grants_of(&self, tab: TabId) -> Vec<&Route> {
        self.routes.iter().filter(|((t, _), _)| *t == tab).map(|(_, r)| r).collect()
    }

    pub fn remembered_all(&self) -> &[Stored] {
        &self.stored
    }
}

fn prompt_text(site: &Site, urn: &str) -> String {
    match urn {
        SHARD => format!("{site} wants to read from the F1R3FLY shard"),
        other => format!("{site} requests {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k1ndl1ng_parse::Level;

    fn knf(src: &str) -> Knf {
        Knf::from_source(src, Level::K1G, &[]).unwrap()
    }

    #[test]
    fn sites_normalise() {
        assert_eq!(Site::of_url("HTTPS://Example.org/a?b").unwrap().as_str(), "https://example.org:443");
        assert_eq!(Site::of_url("http://example.org:8080/").unwrap().as_str(), "http://example.org:8080");
        assert_eq!(
            Site::of_url("https://example.org:443/x"),
            Site::of_url("https://example.org/y")
        );
        assert_ne!(Site::of_url("http://example.org/"), Site::of_url("https://example.org/"));
        assert_eq!(Site::of_url("f1r3://ABCD/proj@^1.2/index.html").unwrap().as_str(), "f1r3://abcd/proj");
        assert!(Site::of_url("javascript:alert(1)").is_none());
    }

    #[test]
    fn plan_follows_the_policy_table() {
        let b = Broker::new(MemGrants::default());
        let site = Site::of_url("https://a.example/").unwrap();
        let p = b.plan(&site, &knf("doc!(1) | net!(2) | shard!(3)"));
        assert!(p.granted(DOC) && p.granted(NET));
        assert!(!p.is_settled());
        assert_eq!(p.pending().count(), 1);
        assert!(!p.granted(SHARD));
        // debug is only for the browser's own pages
        let p2 = b.plan(&site, &Knf::from_source("d!(1)", Level::K1G, &[("d", DEBUG)]).unwrap());
        assert!(!p2.granted(DEBUG));
        let own = Site::of_url("gaze://devtools").unwrap();
        assert!(b.plan(&own, &Knf::from_source("d!(1)", Level::K1G, &[("d", DEBUG)]).unwrap()).granted(DEBUG));
    }

    #[test]
    fn remembered_answers_are_per_grant_hash() {
        let mut b = Broker::new(MemGrants::default());
        let site = Site::of_url("https://a.example/").unwrap();
        let k = knf("shard!(3)");
        let mut p = b.plan(&site, &k);
        b.answer(&mut p, SHARD, true, true).unwrap();
        assert!(p.is_settled() && p.granted(SHARD));
        assert!(b.plan(&site, &k).granted(SHARD), "remembered");
        let mut k2 = k.clone();
        k2.manifest.frame_budget = 1;
        assert!(!b.plan(&site, &k2).is_settled(), "a new manifest is a new request");
        let other = Site::of_url("https://b.example/").unwrap();
        assert!(!b.plan(&other, &k).is_settled(), "and a new site");
    }

    #[test]
    fn routes_attenuate_and_revoke() {
        let mut b = Broker::new(MemGrants::default());
        let site = Site::of_url("https://a.example/").unwrap();
        let mut p = b.plan(&site, &knf("net!(1) | shard!(2)"));
        b.answer(&mut p, SHARD, true, false).unwrap();
        b.install(7, &p);
        assert!(b.check_net(7, "https://a.example/data.json").is_ok());
        assert!(matches!(b.check_net(7, "https://evil.example/"), Err(Refusal::Ask(_))));
        assert!(matches!(b.check_net(7, "file:///etc/passwd"), Err(Refusal::Denied(_))));
        b.allow_net(7, Site::of_url("https://cdn.example/").unwrap());
        assert!(b.check_net(7, "https://cdn.example/x").is_ok());
        assert_eq!(b.check_shard(7, "lookup"), Ok(ShardClass::Read));
        assert!(matches!(b.check_shard(7, "deploy"), Err(Refusal::Ask(_))));
        b.allow_shard(7, ShardClass::Deploy, None).unwrap();
        assert_eq!(b.check_shard(7, "deploy"), Ok(ShardClass::Deploy));
        assert!(b.revoke(7, NET));
        assert_eq!(b.check_net(7, "https://a.example/"), Err(Refusal::Revoked));
        b.close_tab(7);
        assert!(b.grants_of(7).is_empty());
    }

    #[test]
    fn file_store_round_trips() {
        let dir = std::env::temp_dir().join(format!("gaze-broker-{}", std::process::id()));
        let path = dir.join("grants.tsv");
        let site = Site::of_url("https://a.example/").unwrap();
        let k = knf("shard!(3)");
        {
            let mut b = Broker::new(FileGrants::new(&path));
            let mut p = b.plan(&site, &k);
            b.answer(&mut p, SHARD, true, true).unwrap();
            b.install(1, &p);
            b.allow_shard(1, ShardClass::Deploy, Some(k.grant_hash())).unwrap();
        }
        let b = Broker::new(FileGrants::new(&path));
        assert!(b.plan(&site, &k).granted(SHARD));
        assert!(b.remembered_all()[0].classes.contains(&ShardClass::Deploy));
        let _ = std::fs::remove_dir_all(dir);
    }
}
