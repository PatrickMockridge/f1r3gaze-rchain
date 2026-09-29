//! Rendering a published program for the node.
//!
//! Pages send code by hash (spec §9.6): the bridge fetches the `.knf` the
//! hash names, and never signs a term a page assembled from strings. The
//! body prints as rholang with each free import at level `i` written `f{i}`
//! (binders print as `x{n}`/`P{n}`, so there is no capture); the bridge
//! closes it by binding every import:
//!
//! * `rho:gaze:args` — a fresh name carrying the page's ground arguments;
//! * `rho:gaze:return` — the node's `return` name (exploratory deploys);
//! * any other `rho:` URN — a system name, `new fI(\`urn\`)`.
//!
//! Arguments must be ground data without bytes or unforgeable names (neither
//! has a rholang surface form that means the same thing on the node).

use gaze_knf::Knf;
use k1ndl1ng_norm::{Lit, Name, Node, Norm, show};
use k1ndl1ng_parse::Level;

pub const ARGS: &str = "rho:gaze:args";

/// The system names a deploy the browser renders may bind. Every deploy is
/// signed by the user's wallet (the account that pays is the deployer), so
/// anything that carries the deployer's authority is refused: above all
/// `rho:rchain:deployerId`, from which a program could take the vault's auth
/// key and spend the user's funds. A page's program can cost the user phlo,
/// which the consent prompt quotes, and nothing else.
pub const SYSTEM_ALLOWED: &[&str] = &[
    "rho:registry:lookup",
    "rho:registry:insertArbitrary",
    "rho:io:stdout",
    "rho:io:stdoutAck",
    "rho:io:stderr",
    "rho:io:stderrAck",
    "rho:rchain:deployId",
    "rho:block:data",
    "rho:rev:address",
    "rho:crypto:blake2b256Hash",
    "rho:crypto:keccak256Hash",
    "rho:crypto:sha256Hash",
    "rho:crypto:secp256k1Verify",
    "rho:crypto:ed25519Verify",
];
pub const RETURN: &str = "rho:gaze:return";

/// Is `t` expressible on the node: no bytes, no unforgeable names?
pub fn portable(t: &Norm) -> bool {
    let mut procs = vec![t.clone()];
    let mut names: Vec<Name> = Vec::new();
    loop {
        while let Some(n) = names.pop() {
            match n {
                Name::Unforgeable(_) => return false,
                Name::Quote(q) => procs.push(q),
                _ => {}
            }
        }
        let Some(p) = procs.pop() else { return true };
        match p.node() {
            Node::Lit(Lit::Bytes(_)) => return false,
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
            Node::CollRest { items, rest, .. } => {
                procs.extend(items.iter().cloned());
                procs.push(rest.clone());
            }
            _ => {}
        }
    }
}

pub fn render(knf: &Knf, args: &[Norm]) -> Result<String, String> {
    if knf.level > Level::K1G {
        return Err("only K0-K1G programs can be deployed".into());
    }
    if !portable(&knf.body) {
        return Err("program contains bytes or unforgeable names".into());
    }
    for a in args {
        if !portable(a) {
            return Err("arguments may not contain bytes or names".into());
        }
    }
    let mut term = show(&knf.body);
    let mut system = Vec::new();
    let mut ret = None;
    for (i, (_ident, urn)) in knf.manifest.imports.iter().enumerate() {
        let f = format!("f{i}");
        match urn.as_str() {
            ARGS => {
                let a: Vec<String> = args.iter().map(show).collect();
                term = format!("new {f} in {{ {f}!({}) | {term} }}", a.join(", "));
            }
            RETURN => ret = Some(f),
            u if SYSTEM_ALLOWED.contains(&u) => system.push(format!("{f}(`{u}`)")),
            u if u.contains("deployerId") => {
                return Err(format!(
                    "{u} is refused: deploys are signed by your wallet, and the deployer's identity can spend from it"
                ));
            }
            u if u.starts_with("rho:") && !u.starts_with("rho:gaze:") => {
                return Err(format!("system name {u} is not available to pages"));
            }
            other => return Err(format!("capability {other} is not available on the shard")),
        }
    }
    if !system.is_empty() {
        term = format!("new {} in {{ {term} }}", system.join(", "));
    }
    if let Some(f) = ret {
        term = format!("new return, gz in {{ gz!(*return) | for ({f} <- gz) {{ {term} }} }}");
    }
    Ok(term)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_are_bound() {
        let k = Knf::from_source(
            "for (@x <- args) { lookup!(x, *ret) }",
            Level::K1G,
            &[("args", ARGS), ("lookup", "rho:registry:lookup"), ("ret", RETURN)],
        )
        .unwrap();
        let t = render(&k, &[Norm::str("rho:id:abc"), Norm::int(3)]).unwrap();
        assert!(t.starts_with("new return, gz in { gz!(*return) | for ("), "{t}");
        assert!(t.contains("(`rho:registry:lookup`)"), "{t}");
        assert!(t.contains("!(\"rho:id:abc\", 3)"), "{t}");
        // The deployer's identity never reaches a page's program, whatever
        // the program asks for; unknown system names are refused too.
        let spend = Knf::from_source("for (@x <- id) { Nil }", Level::K1G, &[("id", "rho:rchain:deployerId")]).unwrap();
        assert!(render(&spend, &[]).unwrap_err().contains("deployerId"));
        let other = Knf::from_source("x!(1)", Level::K1G, &[("x", "rho:system:whatever")]).unwrap();
        assert!(render(&other, &[]).is_err());
        // Page capabilities never reach the node.
        let bad = Knf::from_source("doc!(1)", Level::K1G, &[]).unwrap();
        assert!(render(&bad, &[]).is_err());
        assert!(render(&k, &[Norm::bytes(b"x")]).is_err());
    }
}
