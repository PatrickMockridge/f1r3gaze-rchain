//! What the wallet will sign for a transfer.
//!
//! Embers prepares transfer contracts from a fixed template
//! (`templates/wallets/transfer.rho` in Embers): look up the wallets
//! environment and call its `transfer` method. A browser does not sign what
//! it cannot read, so before signing it decodes the prepared bytes and
//! requires the term to be exactly that template, filled with the user's own
//! from, to, amount and description. The server chooses only the environment
//! URI and the timestamp, and the call carries no deployer identity, so a
//! dishonest server can at worst waste the deploy's phlo, which is capped.

use crate::address::Address;
use gaze_shard::deploy::DeployData;

/// The template, as Embers renders it (whitespace is not significant).
const TEMPLATE: [&str; 3] = [
    "new rl(`rho:registry:lookup`), walletsCh in {\n    rl!(",
    ", *walletsCh) |\n    for(@(_, wallets) <- walletsCh) {\n        @wallets!(\n            \"transfer\",\n            ",
    ",\n            FROM,\n            TO,\n            AMOUNT,\n            DESCRIPTION\n        )\n    }\n}\n",
];

/// Limits on what a prepared contract may cost and where it may run.
#[derive(Clone, Debug)]
pub struct Limits {
    pub shard_id: String,
    /// Upper bound on `phlo_price × phlo_limit`.
    pub max_fee: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            shard_id: "root".into(),
            max_fee: 10_000_000,
        }
    }
}

/// Remove whitespace outside string literals.
fn squeeze(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut in_str = false;
    let mut esc = false;
    for c in t.chars() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

/// A rholang string literal as Embers renders it.
pub fn rho_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The checked contract: its deploy data, and what the server chose.
#[derive(Clone, Debug)]
pub struct Checked {
    pub deploy: DeployData,
    pub env_uri: String,
    pub timestamp: i64,
}

pub fn check_transfer(
    bytes: &[u8],
    from: &Address,
    to: &Address,
    amount: i64,
    description: Option<&str>,
    limits: &Limits,
) -> Result<Checked, String> {
    let d = DeployData::decode(bytes)?;
    if d.shard_id != limits.shard_id {
        return Err(format!("the contract targets shard {:?}, not {:?}", d.shard_id, limits.shard_id));
    }
    if d.phlo_price <= 0 || d.phlo_limit <= 0 || d.phlo_price.saturating_mul(d.phlo_limit) > limits.max_fee {
        return Err(format!(
            "the contract's fee bound ({} × {}) exceeds the limit {}",
            d.phlo_price, d.phlo_limit, limits.max_fee
        ));
    }
    let t = squeeze(&d.term);
    let p0 = squeeze(TEMPLATE[0]);
    let rest = t.strip_prefix(&p0).ok_or("the contract is not the Embers transfer template")?;
    // The environment URI: `rho:id:...` in backticks.
    let rest = rest.strip_prefix('`').ok_or("expected the environment URI")?;
    let end = rest.find('`').ok_or("unterminated URI")?;
    let uri = &rest[..end];
    if !uri.starts_with("rho:id:") || !uri[7..].chars().all(|c| c.is_ascii_alphanumeric()) || uri.len() < 10 {
        return Err(format!("unexpected environment URI {uri}"));
    }
    let rest = &rest[end + 1..];
    let rest = rest.strip_prefix(&squeeze(TEMPLATE[1])).ok_or("the contract is not the Embers transfer template")?;
    let digits = rest.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(rest.len());
    let timestamp: i64 = rest[..digits].parse().map_err(|_| "expected a timestamp")?;
    let tail = squeeze(
        &TEMPLATE[2]
            .replace("FROM", &rho_string(from.as_str()))
            .replace("TO", &rho_string(to.as_str()))
            .replace("AMOUNT", &amount.to_string())
            .replace("DESCRIPTION", &description.map(rho_string).unwrap_or_else(|| "Nil".into())),
    );
    if rest[digits..] != tail {
        return Err("the contract does not transfer exactly what you asked for".into());
    }
    Ok(Checked {
        deploy: d,
        env_uri: uri.to_string(),
        timestamp,
    })
}

/// Render a transfer contract as Embers does (for tests and mock servers).
pub fn render_transfer(env_uri: &str, timestamp: i64, from: &Address, to: &Address, amount: i64, description: Option<&str>) -> String {
    format!(
        "{}`{env_uri}`{}{timestamp}{}",
        TEMPLATE[0],
        TEMPLATE[1],
        TEMPLATE[2]
            .replace("FROM", &rho_string(from.as_str()))
            .replace("TO", &rho_string(to.as_str()))
            .replace("AMOUNT", &amount.to_string())
            .replace("DESCRIPTION", &description.map(rho_string).unwrap_or_else(|| "Nil".into()))
    )
}
