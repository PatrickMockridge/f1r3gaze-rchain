//! The bridge: shared state ([`Bridge`]) and one [`ShardService`] per tab.

use crate::chain::{self, Blocks};
use crate::deploy::{DeployData, SignedDeploy, public_key, sign_for};
use crate::expr::to_norm;
use crate::history;
use crate::keys::fresh_key;
use crate::node::{Node, NodeDialect};
use crate::pos;
use crate::site::{SiteAddr, SiteManifest};
use crate::txn;
use crate::term::render;
use gaze_blob::{BlobSource, Blobs};
use gaze_knf::Knf;
use gaze_net::{Http, Pool, hex};
use k1ndl1ng_norm::{CollKind, Lit, Name, Node as NNode, Norm};
use k256::ecdsa::SigningKey;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct ShardConfig {
    /// Read-only observers, run by distinct operators for the quorum rung.
    pub observers: Vec<String>,
    /// Where deploys go.
    pub validator: String,
    pub shard_id: String,
    /// Agreeing observers needed for the quorum rung.
    pub quorum: usize,
    pub phlo_price: i64,
    /// The profile's user id, part of every site key's identity.
    pub user: String,
    /// Which node the observers and validator speak to.
    pub dialect: NodeDialect,
    /// The phlo bound quoted when the node offers no cost estimate (rchain).
    pub phlo_limit: i64,
    /// The node's **admin** HTTP base (rnode's port 40405), for the operations
    /// that live there rather than on the API port — today just `propose`.
    ///
    /// `None` by default: the admin listener is bound to loopback and its port
    /// is a deployment's business, so it is named rather than guessed.
    pub admin: Option<String>,
}

impl Default for ShardConfig {
    fn default() -> Self {
        ShardConfig {
            observers: vec!["http://localhost:40453".into()],
            validator: "http://localhost:40403".into(),
            shard_id: "root".into(),
            quorum: 2,
            phlo_price: 1,
            user: "default".into(),
            dialect: NodeDialect::F1r3fly,
            phlo_limit: NodeDialect::F1r3fly.default_phlo_limit(),
            admin: None,
        }
    }
}

/// How much to trust an answer (spec §9.5).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Rung {
    Node,
    Quorum,
    Proof,
    Replayed,
}

impl Rung {
    pub fn name(self) -> &'static str {
        match self {
            Rung::Node => "node",
            Rung::Quorum => "quorum",
            Rung::Proof => "proof",
            Rung::Replayed => "replayed",
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Collapse a node answer (a list of values) to one term.
fn collapse(vs: &[Value]) -> Norm {
    match vs {
        [] => Norm::nil(),
        [one] => to_norm(one),
        many => Norm::list(many.iter().map(to_norm).collect()),
    }
}

/// Notifies subscribers when a block is finalized: from `block-finalised`
/// events on `/ws/events` where the node has a stream, or by polling the last
/// finalized block where it does not (rchain). Subscribers that fall silent
/// are dropped.
pub struct EventHub {
    subs: Mutex<Vec<Sender<()>>>,
}

impl EventHub {
    /// An event-stream source (the f1r3fly dialect).
    pub fn start(url: String) -> Arc<EventHub> {
        let hub = Arc::new(EventHub { subs: Mutex::new(Vec::new()) });
        let h = Arc::clone(&hub);
        let _ = std::thread::Builder::new().name("gaze-shard-events".into()).spawn(move || {
            let mut backoff = 1u64;
            loop {
                if Arc::strong_count(&h) == 1 {
                    return; // nobody left
                }
                if let Ok((mut ws, _)) = tungstenite::connect(&url) {
                    backoff = 1;
                    while let Ok(msg) = ws.read() {
                        if let Ok(t) = msg.to_text() {
                            let v: Value = serde_json::from_str(t).unwrap_or(Value::Null);
                            if v.get("event").and_then(|e| e.as_str()) == Some("block-finalised") {
                                h.notify();
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
            }
        });
        hub
    }

    /// A poll source, for a node with no event stream (the rchain dialect):
    /// notify when the last finalized block hash changes, backing off while
    /// the chain has no finalized block yet (a fresh node 400s there).
    pub fn start_polling(node: Node, tick: Duration) -> Arc<EventHub> {
        let hub = Arc::new(EventHub { subs: Mutex::new(Vec::new()) });
        let h = Arc::clone(&hub);
        let _ = std::thread::Builder::new().name("gaze-shard-events".into()).spawn(move || {
            let mut backoff = tick;
            let mut last: Option<String> = None;
            loop {
                if Arc::strong_count(&h) == 1 {
                    return; // nobody left
                }
                match node.last_finalized() {
                    Ok((hash, _)) => {
                        if last.as_deref() != Some(hash.as_str()) {
                            last = Some(hash);
                            h.notify();
                        }
                        backoff = tick;
                    }
                    Err(_) => backoff = (backoff * 2).min(Duration::from_secs(60)),
                }
                std::thread::sleep(backoff);
            }
        });
        hub
    }

    pub fn subscribe(&self) -> Receiver<()> {
        let (tx, rx) = channel();
        if let Ok(mut s) = self.subs.lock() {
            s.push(tx);
        }
        rx
    }

    pub fn notify(&self) {
        if let Ok(mut s) = self.subs.lock() {
            s.retain(|t| t.send(()).is_ok());
        }
    }
}

/// Who pays for work on the node. On this protocol the account charged for a
/// deploy is its deployer, so the payer's key signs every deploy the browser
/// makes; what a deploy may do with that identity is limited by the terms the
/// bridge renders (see [`crate::term`]: no `rho:rchain:deployerId`).
pub trait Payer: Send + Sync + 'static {
    /// The signing key and its account address.
    fn payer(&self) -> Result<(SigningKey, String), String>;
    /// The account's balance, if known, for the consent prompt.
    fn balance(&self) -> Option<u64> {
        None
    }
}

/// A fixed key: for tests and for scripted agents that bring their own key.
pub struct KeyPayer {
    pub key: SigningKey,
    pub address: String,
}

impl Payer for KeyPayer {
    fn payer(&self) -> Result<(SigningKey, String), String> {
        Ok((self.key.clone(), self.address.clone()))
    }
}

/// Shared by every tab of a profile.
pub struct Bridge {
    pub cfg: ShardConfig,
    pub http: Http,
    pub pool: Pool,
    /// Who pays: the key that signs every deploy, and its account.
    pub payer: Arc<dyn Payer>,
    pub blobs: Arc<Blobs>,
    /// Highest finalized block at which each binding was seen (freshness).
    seen: Mutex<BTreeMap<String, i64>>,
    pub events: Option<Arc<EventHub>>,
}

impl Bridge {
    pub fn new(cfg: ShardConfig, http: Http, pool: Pool, payer: Arc<dyn Payer>, blobs: Arc<Blobs>) -> Arc<Bridge> {
        let events = cfg.observers.first().map(|o| {
            let node = Node::new(o, cfg.dialect, http.clone());
            match node.events_url() {
                Some(url) => EventHub::start(url),
                // No event stream: poll the last finalized block instead.
                None => EventHub::start_polling(node, Duration::from_secs(5)),
            }
        });
        Arc::new(Bridge {
            cfg,
            http,
            pool,
            payer,
            blobs,
            seen: Mutex::new(BTreeMap::new()),
            events,
        })
    }

    fn observers(&self) -> Vec<Node> {
        self.cfg.observers.iter().map(|o| Node::new(o, self.cfg.dialect, self.http.clone())).collect()
    }
    fn validator(&self) -> Node {
        Node::new(&self.cfg.validator, self.cfg.dialect, self.http.clone())
    }

    /// Ask every observer the same question at the same finalized block and
    /// grade the answer: one observer is `node`; `quorum` needs `k` identical
    /// answers from distinct observers.
    ///
    /// Generic in the answer so identifier-addressed reads can be graded on
    /// their parsed type. For `Norm` this is unchanged behaviour: `Norm`'s own
    /// `PartialEq` *is* the encoding comparison, plus an `Arc` fast path.
    fn graded<T: Clone + PartialEq>(
        &self,
        ask: &dyn Fn(&Node, &str) -> Result<T, String>,
    ) -> Result<(Rung, T, i64), String> {
        let obs = self.observers();
        let first = obs.first().ok_or("no observers configured")?;
        let (block, num) = first.last_finalized()?;
        let mut answers: Vec<T> = Vec::new();
        let mut errors = Vec::new();
        for o in &obs {
            match ask(o, &block) {
                Ok(v) => answers.push(v),
                Err(e) => errors.push(e),
            }
        }
        let Some(a0) = answers.first().cloned() else {
            return Err(errors.join("; "));
        };
        if obs.len() == 1 {
            return Ok((Rung::Node, a0, num));
        }
        let mut best: Option<(usize, T)> = None;
        for a in &answers {
            let n = answers.iter().filter(|b| *b == a).count();
            if best.as_ref().is_none_or(|(m, _)| n > *m) {
                best = Some((n, a.clone()));
            }
        }
        let (n, v) = best.expect("nonempty");
        if n >= self.cfg.quorum.max(2) {
            Ok((Rung::Quorum, v, num))
        } else {
            Err(format!("observers disagree at block {block} ({n} of {} agree)", obs.len()))
        }
    }

    /// One observer for a read that is *not* gradeable — a head-relative or
    /// node-local answer, where two observers legitimately differ.
    fn reader(&self) -> Result<Node, String> {
        self.observers().into_iter().next().ok_or_else(|| "no observers configured".to_string())
    }

    /// The guard for the rchain-only surface. It reads the dialect, so nothing
    /// is sent before it passes.
    fn rchain_only(&self, what: &str) -> Result<(), String> {
        if self.cfg.dialect != NodeDialect::Rchain {
            return Err(format!("the {what} are the rchain dialect's; f1r3fly has no such route"));
        }
        Ok(())
    }

    fn check_fresh(&self, binding: &str, num: i64) -> Result<(), String> {
        let mut seen = self.seen.lock().map_err(|_| "poisoned")?;
        let e = seen.entry(binding.to_string()).or_insert(num);
        if num < *e {
            return Err(format!("stale answer: block {num} is older than {e}, already seen for {binding}"));
        }
        *e = num;
        Ok(())
    }

    pub fn lookup(&self, uri: &str) -> Result<(Rung, Norm), String> {
        let (rung, v, num) = self.graded(&|o, b| o.registry(uri, Some(b)).map(|(d, _, _)| collapse(&d)))?;
        self.check_fresh(uri, num)?;
        Ok((rung, v))
    }

    pub fn read_private(&self, hex: &str) -> Result<(Rung, Norm), String> {
        let (rung, v, _) = self.graded(&|o, b| o.data_at_private(hex, b).map(|d| collapse(&d)))?;
        Ok((rung, v))
    }

    pub fn explore(&self, term: &str) -> Result<(Rung, Norm), String> {
        let (rung, v, _) = self.graded(&|o, b| o.explore(term, b).map(|(d, _, _)| collapse(&d)))?;
        Ok((rung, v))
    }

    /// Resolve a site's manifest (with freshness).
    pub fn resolve_site(&self, addr: &SiteAddr) -> Result<(Rung, SiteManifest), String> {
        let uri = addr.registry_uri();
        let (rung, v, num) = self.graded(&|o, b| o.registry(&uri, Some(b)).map(|(d, _, _)| collapse(&d)))?;
        self.check_fresh(&addr.binding(), num)?;
        Ok((rung, SiteManifest::from_norm(&v)?))
    }

    /// One file of a site, verified by hash.
    pub fn site_file(&self, m: &SiteManifest, path: &str) -> Result<(String, Vec<u8>), String> {
        let (name, h) = m.file_for(path).ok_or_else(|| format!("no such file: {path}"))?;
        Ok((name.to_string(), self.blobs.get(&h, &m.mirrors)?))
    }

    /// The deploy term for the `.knf` a page named by hash.
    pub fn render_by_hash(&self, h: &[u8; 32], args: &[Norm]) -> Result<String, String> {
        let bytes = self.blobs.get(h, &[])?;
        let knf = Knf::decode(&bytes).map_err(|e| format!("not a .knf: {e:?}"))?;
        render(&knf, args)
    }

    pub fn estimate(&self, term: &str, key: &SigningKey) -> Result<u64, String> {
        // rchain has no estimate-cost endpoint: quote the configured bound
        // instead, and the consent prompt says "up to".
        if self.cfg.dialect == NodeDialect::Rchain {
            return Ok(self.cfg.phlo_limit.max(0) as u64);
        }
        let obs = self.observers();
        obs.first().ok_or("no observers")?.estimate_cost(term, &hex(&public_key(key)))
    }

    pub fn sign_and_deploy(&self, key: &SigningKey, term: &str, phlo_limit: i64) -> Result<SignedDeploy, String> {
        let v = self.validator();
        let (_, num) = v.last_finalized()?;
        // The node refuses a deploy whose shard id is not its own; rchain
        // reports its own full id on `/api/status`, so read it rather than
        // trusting the profile's spelling.
        let shard_id = match self.cfg.dialect {
            NodeDialect::F1r3fly => self.cfg.shard_id.clone(),
            NodeDialect::Rchain => v.shard_id().unwrap_or_else(|_| self.cfg.shard_id.clone()),
        };
        let now = now_ms();
        let d = sign_for(
            key,
            DeployData {
                term: term.to_string(),
                timestamp: now,
                phlo_price: self.cfg.phlo_price,
                phlo_limit,
                valid_after_block_number: num,
                shard_id,
                // Replay protection besides the timestamp and block bound. The
                // rchain proto has no field for it, so it is omitted there.
                expiration_timestamp: match self.cfg.dialect {
                    NodeDialect::F1r3fly => Some(now + 5 * 60 * 1000),
                    NodeDialect::Rchain => None,
                },
            },
            self.cfg.dialect,
        )?;
        self.validator().deploy(&d)?;
        Ok(d)
    }

    pub fn finalization(&self, id: &str) -> Result<(String, Option<String>), String> {
        self.validator().finalization(id)
    }

    /// The REV balance of `addr`, read by an exploratory deploy of the native
    /// `revVault` `getBalance`, graded like every other read.
    pub fn rev_balance(&self, addr: &str) -> Result<(Rung, u64), String> {
        // Without this, an f1r3fly bridge would ship a term naming
        // `rho:rchain:revVault` to an f1r3fly node.
        self.rchain_only("REV balance reads")?;
        let term = crate::wallet::balance_term(addr);
        let (rung, v, _) = self.graded(&|o, b| o.explore(&term, b).map(|(d, _, _)| collapse(&d)))?;
        Ok((rung, crate::wallet::parse_balance(&v)?))
    }

    /// Transfer `drops` REV from the payer's vault to `addr`, as a signed
    /// deploy. The source is the payer's `deployerId`, so the payer's key is
    /// the only authority involved.
    pub fn rev_transfer(&self, to: &str, drops: i64) -> Result<SignedDeploy, String> {
        if self.cfg.dialect != NodeDialect::Rchain {
            return Err("REV transfers are the rchain dialect's; the f1r3fly wallet goes through Embers".into());
        }
        let (key, _) = self.payer.payer()?;
        let term = crate::wallet::transfer_term(to, drops);
        self.sign_and_deploy(&key, &term, crate::wallet::TRANSFER_PHLO_LIMIT)
    }

    /// Fund `addr` from the node's devnet faucet: `(deploy id, drops)`.
    pub fn faucet(&self, addr: &str) -> Result<(String, i64), String> {
        self.validator().faucet(addr)
    }

    // --- chain reads (rchain only) -----------------------------------------
    //
    // Graded iff the answer is a function of an identifier the caller named, or
    // is read at a block the grader pins. A head-relative or node-local answer
    // is one observer's, honestly labelled `node` -- grading it would let two
    // observers at different heads read as a disagreement.

    /// A block and its deploys, by hash.
    pub fn block(&self, hash: &str) -> Result<(Rung, chain::BlockInfo), String> {
        self.rchain_only("chain reads")?;
        let (rung, v, _) = self.graded(&|o, _| o.block(hash))?;
        Ok((rung, v))
    }

    /// The block containing the deploy with this signature.
    pub fn find_deploy(&self, id: &str) -> Result<(Rung, chain::LightBlockInfo), String> {
        self.rchain_only("chain reads")?;
        let (rung, v, _) = self.graded(&|o, _| o.find_deploy(id))?;
        Ok((rung, v))
    }

    /// The newest blocks named by `spec`. Head-relative, so not graded; the
    /// page-facing depth cap is applied by the caller.
    pub fn blocks(&self, spec: Blocks) -> Result<(Rung, Vec<chain::LightBlockInfo>), String> {
        self.rchain_only("chain reads")?;
        Ok((Rung::Node, self.reader()?.blocks(spec, None)?))
    }

    /// Whether this node has finalized the block with this hash. **Not**
    /// graded: finality is a local view, and a slower observer legitimately
    /// answers `false` for a block another has already finalized.
    pub fn is_finalized(&self, hash: &str) -> Result<(Rung, bool), String> {
        self.rchain_only("chain reads")?;
        Ok((Rung::Node, self.reader()?.is_finalized(hash)?))
    }

    /// The deploys this node has accepted and not yet included.
    pub fn pool(&self) -> Result<(Rung, Vec<chain::PooledDeploy>), String> {
        self.rchain_only("chain reads")?;
        Ok((Rung::Node, self.reader()?.pool()?))
    }

    /// What this node will do.
    pub fn capabilities(&self) -> Result<(Rung, chain::NodeCapabilities), String> {
        self.rchain_only("chain reads")?;
        Ok((Rung::Node, self.reader()?.capabilities()?))
    }

    /// The shards this node serves.
    pub fn shards(&self) -> Result<(Rung, chain::Shards), String> {
        self.rchain_only("chain reads")?;
        Ok((Rung::Node, self.reader()?.shards()?))
    }

    /// The REV transfers in the block with this hash — the per-block primitive
    /// an address history is walked out of. Graded, because a block hash *is*
    /// the identifier the answer is a function of.
    pub fn transactions(&self, hash: &str) -> Result<(Rung, Vec<history::Transfer>), String> {
        self.rchain_only("transaction reads")?;
        let (rung, v, _) = self.graded(&|o, _| o.transactions(hash))?;
        Ok((rung, v))
    }

    /// An address's recent REV transfers, by walking the node's own block
    /// reports.
    ///
    /// Head-relative, so **not** graded: the answer depends on where the chain
    /// is, and two observers at different heads legitimately differ — the same
    /// reason [`Bridge::blocks`] is one observer's. The walk is bounded by
    /// [`history::HISTORY_MAX_BLOCKS`] and by the chain's own length, and the
    /// result reports how much of it could actually be read.
    ///
    /// The tip is the node's **newest block**, not its finalized fringe. A
    /// fringe is the right anchor for a *graded* read — it is what two
    /// observers can agree on — but it lags the head, and it lags by an amount
    /// that is the node's business rather than the caller's, so anchoring here
    /// would leave a transfer the user just made out of its own history. This
    /// read is deliberately one observer's; it is freshness, not agreement,
    /// that makes it useful.
    pub fn transfer_history(&self, addr: &str, blocks: i32) -> Result<(Rung, history::History), String> {
        self.rchain_only("transfer history reads")?;
        let n = self.reader()?;
        let tip = n
            .blocks(Blocks::Depth(1), None)?
            .first()
            .map(|b| b.block_number)
            .ok_or("no blocks on this node")?;
        let mut out = history::History {
            transfers: Vec::new(),
            blocks_read: 0,
            blocks_unread: 0,
        };
        // The first refusal, kept so that a walk which read *nothing* can say
        // why. "No transfers" and "no transfers in the blocks I could read" are
        // different answers, and a walk that read none is neither: it is a
        // failure that happens to look like an empty history.
        let mut refused: Option<String> = None;
        for (start, end) in history::windows(tip, blocks, history::WALK_WINDOW) {
            let bs = match n.blocks(Blocks::Range(start, end), None) {
                Ok(bs) => bs,
                // A window that cannot be listed is a window of unread blocks.
                Err(e) => {
                    refused.get_or_insert(e);
                    out.blocks_unread += end - start + 1;
                    continue;
                }
            };
            // The node answers a range oldest-first; the walk goes newest-first.
            for b in bs.iter().rev() {
                match read_block(&n, &b.block_hash) {
                    Ok(ts) => {
                        out.blocks_read += 1;
                        for mut t in ts {
                            if t.from_addr == addr || t.to_addr == addr {
                                t.block_hash = b.block_hash.clone();
                                t.block_number = b.block_number;
                                out.transfers.push(t);
                            }
                        }
                    }
                    Err(e) => {
                        refused.get_or_insert(e);
                        out.blocks_unread += 1;
                    }
                }
            }
        }
        if out.blocks_read == 0 && out.blocks_unread > 0 {
            return Err(refused.unwrap_or_else(|| format!("no block in the last {blocks} could be read")));
        }
        Ok((Rung::Node, out))
    }

    // --- staking reads (rchain only) ---------------------------------------

    /// The epoch, the active set and every staged withdrawal. Head-relative.
    pub fn pos_status(&self) -> Result<(Rung, pos::PosStatus), String> {
        self.rchain_only("staking reads")?;
        Ok((Rung::Node, self.reader()?.pos_status()?))
    }

    /// One delegator's positions. The route takes no block, so it is answered
    /// at one observer's head -- not gradeable.
    pub fn pos_delegations(&self, key: &str) -> Result<(Rung, Vec<pos::DelegatorPosition>), String> {
        self.rchain_only("staking reads")?;
        Ok((Rung::Node, self.reader()?.pos_delegations(key)?))
    }

    // The native `pos` reads run as an exploratory deploy, so the grader pins
    // one block for every observer and they *are* gradeable.

    /// `getBonds` — every validator's stake, with delegations aggregated in.
    pub fn pos_bonds(&self) -> Result<(Rung, Norm), String> {
        self.rchain_only("staking reads")?;
        let term = pos::bonds_term();
        let (rung, n, _) = self.graded(&|o, b| o.explore(&term, b).and_then(|(d, _, _)| pos::bonds_from(&d)))?;
        Ok((rung, n))
    }

    /// `getActiveValidators` — the set drawing blocks this epoch.
    pub fn pos_active_validators(&self) -> Result<(Rung, Norm), String> {
        self.rchain_only("staking reads")?;
        let term = pos::active_validators_term();
        let (rung, n, _) =
            self.graded(&|o, b| o.explore(&term, b).and_then(|(d, _, _)| pos::keys_from(&d, "getActiveValidators")))?;
        Ok((rung, n))
    }

    /// `getTrusted` — the keys a stakeholder has admitted to the pool.
    pub fn pos_trusted(&self) -> Result<(Rung, Norm), String> {
        self.rchain_only("staking reads")?;
        let term = pos::trusted_term();
        let (rung, n, _) = self.graded(&|o, b| o.explore(&term, b).and_then(|(d, _, _)| pos::keys_from(&d, "getTrusted")))?;
        Ok((rung, n))
    }

    /// `getDelegations` — one delegator's positions, read in rholang rather
    /// than over HTTP. Kept for parity; the HTTP route carries two more fields.
    pub fn pos_delegations_native(&self, key: &str) -> Result<(Rung, Norm), String> {
        self.rchain_only("staking reads")?;
        pos::validate_key(key)?;
        let term = pos::delegations_term(key);
        let (rung, n, _) = self.graded(&|o, b| o.explore(&term, b).and_then(|(d, _, _)| pos::delegations_from(&d)))?;
        Ok((rung, n))
    }

    // --- staking writes (rchain only) --------------------------------------
    //
    // Neither write names a key: the node derives the validator from the
    // payer's `*deployerId`, so a caller can only ever stake or unstake its
    // own REV. That is what bounds a page's reach (see the `stake` verb).

    /// Self-bond `drops` at the validator the payer's key signs as.
    pub fn pos_bond(&self, drops: i64) -> Result<SignedDeploy, String> {
        self.rchain_only("staking writes")?;
        let (key, _) = self.payer.payer()?;
        let term = pos::bond_term(drops);
        self.sign_and_deploy(&key, &term, pos::STAKE_PHLO_LIMIT)
    }

    /// Stage the unbond. Refused while the validator carries delegations.
    pub fn pos_withdraw(&self) -> Result<SignedDeploy, String> {
        self.rchain_only("staking writes")?;
        let (key, _) = self.payer.payer()?;
        let term = pos::withdraw_term();
        self.sign_and_deploy(&key, &term, pos::STAKE_PHLO_LIMIT)
    }

    /// Stake `drops` of the payer's REV on the operator `key_hex`.
    ///
    /// This **names a key**, unlike a bond: the principal stays the delegator's,
    /// but it is held in the operator's pool entry and shares that operator's
    /// slash risk and its pro-rata rewards.
    pub fn pos_delegate(&self, key_hex: &str, drops: i64) -> Result<SignedDeploy, String> {
        self.rchain_only("staking writes")?;
        pos::validate_key(key_hex)?;
        let (key, _) = self.payer.payer()?;
        let term = pos::delegate_term(key_hex, drops);
        self.sign_and_deploy(&key, &term, pos::STAKE_PHLO_LIMIT)
    }

    /// Stage the exit from a delegation. Signed by the delegator, so only the
    /// key that staked can take it back.
    pub fn pos_undelegate(&self, key_hex: &str) -> Result<SignedDeploy, String> {
        self.rchain_only("staking writes")?;
        pos::validate_key(key_hex)?;
        let (key, _) = self.payer.payer()?;
        let term = pos::undelegate_term(key_hex);
        self.sign_and_deploy(&key, &term, pos::STAKE_PHLO_LIMIT)
    }

    /// Publish a site manifest: one deploy that sends it to the public channel
    /// the site's address names.
    ///
    /// rchain only. f1r3fly keeps a manifest in its registry
    /// (`rho:registry:insertSigned`), which is a different mechanism and is not
    /// wired here either — so this is a refusal, not a regression.
    ///
    /// The manifest is what makes the address resolve; [`Self::publish_blobs`]
    /// is what makes the site *load*, and a site with neither a mirror nor
    /// published blobs resolves and then fails.
    pub fn publish_site(&self, uri: &str, m: &SiteManifest) -> Result<SignedDeploy, String> {
        self.rchain_only("site publishing")?;
        let (key, _) = self.payer.payer()?;
        let term = crate::site::publish_term(uri, m);
        self.sign_and_deploy(&key, &term, crate::site::PUBLISH_PHLO_LIMIT)
    }

    /// Publish files on-chain in F1R3Drive's layout, under the root this
    /// client's own reader watches — so a published site needs no mirror.
    ///
    /// One deploy carries every file as a parallel send. A file over the
    /// reader's limit is refused with a reason rather than truncated, because a
    /// truncated blob would pass its hash check and serve wrong content.
    pub fn publish_blobs(&self, files: &[([u8; 32], Vec<u8>)]) -> Result<SignedDeploy, String> {
        self.rchain_only("site publishing")?;
        let (key, _) = self.payer.payer()?;
        let term = crate::site::blobs_term(files)?;
        self.sign_and_deploy(&key, &term, crate::site::BLOB_PHLO_LIMIT)
    }

    /// A deploy's outcome, including the value it produced.
    pub fn deploy_outcome(&self, id: &str) -> Result<chain::DeployOutcome, String> {
        self.validator().deploy_outcome(id)
    }

    /// Force a block — the operation that moves a node off `--autopropose`.
    ///
    /// It lives on the node's **admin** listener, so it needs
    /// [`ShardConfig::admin`]. There is no default: the admin port is a
    /// deployment's business and the listener acts with the node's own key, so
    /// guessing an address would aim a privileged request somewhere unintended.
    ///
    /// Not a page verb, and not on `f1r3fly` — see [`Node::propose`].
    pub fn propose(&self) -> Result<String, String> {
        self.admin()?.propose()
    }

    /// The node's admin listener, when one is configured.
    ///
    /// It is where the operations that act with the node's **own key** live: `propose`, and the
    /// gateway's cross-shard transactions. There is no default, because the admin port is a
    /// deployment's business and guessing would aim a privileged request somewhere unintended.
    fn admin(&self) -> Result<Node, String> {
        let a = self
            .cfg
            .admin
            .as_ref()
            .ok_or("no admin address: set `admin = http://host:40405` in settings.conf")?;
        Ok(Node::new(a, self.cfg.dialect, self.http.clone()))
    }

    /// Open a cross-shard transaction on the node's gateway.
    ///
    /// **A transaction is the node moving its own funds.** The gateway signs every leg with its
    /// validator key and escrows out of that key's own REV account, so this is not a client-signed
    /// transfer: it asks a node that is a member of several shards to move REV between them. A node
    /// that is a member of one shard has no gateway, and the route answers 404.
    pub fn txn_open(&self, req: &txn::TxnRequest) -> Result<txn::TxnRecord, String> {
        self.admin()?.txn_open(req)
    }

    /// One transaction's record, by id.
    pub fn txn_status(&self, id: &str) -> Result<txn::TxnRecord, String> {
        self.admin()?.txn_status(id)
    }

    /// The transactions this node still holds — in flight, and decided ones it keeps for recovery.
    pub fn txn_list(&self) -> Result<txn::TxnList, String> {
        self.admin()?.txn_list()
    }

    /// Wait for a deploy to settle and return its outcome.
    ///
    /// `Err` here is a *failed* deploy or a timeout — deliberately **not** a
    /// refusal, which is a successful deploy whose program declined.
    fn settle_outcome(&self, id: &str) -> Result<chain::DeployOutcome, String> {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(120) {
            let o = self.deploy_outcome(id)?;
            match o.state.as_str() {
                "Pending" => std::thread::sleep(Duration::from_secs(3)),
                "Failed" => {
                    return Err(o.error.unwrap_or_else(|| "the deploy failed, with no reason given".into()));
                }
                _ => return Ok(o),
            }
        }
        Err(format!("deploy {id} did not settle within 120 s"))
    }

    /// Wait for a deploy that only *does* something to settle: `Ok(())` once it
    /// is included and ran.
    ///
    /// Use this for a deploy with no reply to read — a site publish sends to a
    /// channel and terminates, so it produces nothing on its `deployId`. Asking
    /// [`Self::pos_settle`] to parse one would report "answered nothing" on a
    /// success.
    pub fn settle(&self, id: &str) -> Result<(), String> {
        self.settle_outcome(id).map(|_| ())
    }

    /// Wait for a write to settle and report **what the chain said**: `Ok(())`
    /// if the node did the thing, `Err(reason)` if it refused.
    ///
    /// The outer `Result` is the transport and the timeout; the inner one is
    /// the chain's answer. They are kept apart because a refusal is *not* a
    /// failure — the deploy succeeded and the program declined — and collapsing
    /// them would make "the bond was refused" read like "the request broke".
    pub fn pos_settle(&self, id: &str) -> Result<Result<(), String>, String> {
        self.rchain_only("staking writes")?;
        Ok(pos::reply_from(&self.settle_outcome(id)?.result))
    }
}

// ---------------------------------------------------------------------------

/// What the service wants the tab to do.
#[derive(Clone, Debug)]
pub enum ShardOut {
    /// `chan!(datum)` in the next frame.
    Reply { chan: Name, datum: Norm },
    /// Mint a session name labelled `label` and reply
    /// `("ok", rung, *session)` on `chan`.
    BindSession { chan: Name, label: String },
}

/// Something only the user can decide.
#[derive(Clone, Debug)]
pub struct Prompt {
    pub id: u64,
    pub text: String,
}

enum Pending {
    Deploy { term: String, cost: u64, ret: Option<Name> },
    Session { uri: String, ret: Option<Name> },
}

struct Session {
    key: SigningKey,
    uri: String,
}

struct Shared {
    out: Vec<ShardOut>,
    prompts: Vec<(Prompt, Pending)>,
    sessions: BTreeMap<String, Session>,
    watches: Vec<(String, Name, Vec<u8>)>,
}

/// One tab's shard capability.
pub struct ShardService {
    bridge: Arc<Bridge>,
    site: String,
    shared: Arc<Mutex<Shared>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    next: u64,
    watcher: bool,
}

/// One block's transfers, waiting out the rate limiter.
///
/// The walk is many requests against a route that shares the node's deploy
/// limiter, so a 429 is the expected answer rather than an error worth
/// surfacing; anything else is left for the caller to count as unread.
fn read_block(n: &Node, hash: &str) -> Result<Vec<history::Transfer>, String> {
    let mut attempt = 0;
    loop {
        match n.transactions(hash) {
            Err(e) if crate::node::is_rate_limited(&e) && attempt < 8 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(250));
            }
            other => return other,
        }
    }
}

fn ok3(rung: &str, v: Norm) -> Norm {
    Norm::tuple(vec![Norm::str("ok"), Norm::str(rung), v])
}
fn err3(code: &str, d: &str) -> Norm {
    Norm::tuple(vec![Norm::str("err"), Norm::str(code), Norm::str(d)])
}
fn as_name(n: &Norm) -> Option<Name> {
    match n.node() {
        NNode::Eval(x) => Some(x.clone()),
        _ => None,
    }
}
fn hash_arg(n: &Norm) -> Option<[u8; 32]> {
    match n.as_lit() {
        Some(Lit::Bytes(b)) => b.to_vec().try_into().ok(),
        _ => {
            let s = n.as_str()?;
            gaze_net::unhex(s.strip_prefix("blake2b-256:").unwrap_or(s))?.try_into().ok()
        }
    }
}

/// A validator key from a page, as hex. A page may hand it either as a 65-byte
/// array or as its hex spelling; either way it is **validated before a term is
/// built**, so a page cannot put anything but a real key into the term.
fn key_arg(n: &Norm) -> Option<String> {
    let hex = match n.as_lit() {
        Some(Lit::Bytes(b)) if b.len() == 65 => gaze_net::hex(b),
        _ => n.as_str()?.to_string(),
    };
    crate::pos::validate_key(&hex).ok()?;
    Some(hex)
}

impl ShardService {
    pub fn new(bridge: Arc<Bridge>, site: &str, wake: Arc<dyn Fn() + Send + Sync>) -> ShardService {
        ShardService {
            bridge,
            site: site.to_string(),
            shared: Arc::new(Mutex::new(Shared {
                out: Vec::new(),
                prompts: Vec::new(),
                sessions: BTreeMap::new(),
                watches: Vec::new(),
            })),
            wake,
            next: 0,
            watcher: false,
        }
    }

    fn push(shared: &Arc<Mutex<Shared>>, wake: &Arc<dyn Fn() + Send + Sync>, o: ShardOut) {
        if let Ok(mut s) = shared.lock() {
            s.out.push(o);
        }
        wake();
    }

    fn job(&self, f: impl FnOnce(&Bridge) -> Option<ShardOut> + Send + 'static) {
        let (b, sh, w) = (Arc::clone(&self.bridge), Arc::clone(&self.shared), Arc::clone(&self.wake));
        self.bridge.pool.spawn(move || {
            if let Some(o) = f(&b) {
                Self::push(&sh, &w, o);
            }
        });
    }

    /// A request from the page, on the `shard` capability (`label ==
    /// "rho:gaze:shard"`) or on a session name the bridge minted. The shell
    /// has already checked the verb class against the broker.
    pub fn request(&mut self, label: &str, args: &[Norm]) {
        let ret = args.last().and_then(as_name);
        if label != "rho:gaze:shard" {
            return self.session_send(label, args);
        }
        let verb = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
        let reply = move |d: Norm| ret.clone().map(|chan| ShardOut::Reply { chan, datum: d });
        match verb.as_str() {
            "lookup" => {
                let Some(uri) = args.get(1).and_then(|u| u.as_str()).map(str::to_string) else {
                    return self.now(reply(err3("type", "lookup")));
                };
                self.job(move |b| {
                    reply(match b.lookup(&uri) {
                        Ok((r, v)) => ok3(r.name(), v),
                        Err(e) => err3("shard", &e),
                    })
                });
            }
            "read" => {
                let target = args.get(1).cloned().unwrap_or_else(Norm::nil);
                self.job(move |b| {
                    let r = match (target.as_str(), target.as_coll(CollKind::Tuple)) {
                        (Some(s), _) if s.starts_with("rho:") => b.lookup(s),
                        (Some(h), _) => b.read_private(h),
                        (None, Some([_, _, h])) => b.read_private(h.as_str().unwrap_or("")),
                        _ => Err("read wants a URN or an unforgeable name".into()),
                    };
                    reply(match r {
                        Ok((r, v)) => ok3(r.name(), v),
                        Err(e) => err3("shard", &e),
                    })
                });
            }
            "explore" => {
                let (Some(h), pargs) = (args.get(1).and_then(hash_arg), args.get(2).cloned()) else {
                    return self.now(reply(err3("type", "explore wants a program hash")));
                };
                let pargs: Vec<Norm> = pargs.and_then(|l| l.as_coll(CollKind::List).map(|x| x.to_vec())).unwrap_or_default();
                self.job(move |b| {
                    reply(match b.render_by_hash(&h, &pargs).and_then(|t| b.explore(&t)) {
                        Ok((r, v)) => ok3(r.name(), v),
                        Err(e) => err3("shard", &e),
                    })
                });
            }
            "deploy" => {
                let (Some(h), pargs) = (args.get(1).and_then(hash_arg), args.get(2).cloned()) else {
                    return self.now(reply(err3("type", "deploy wants a program hash")));
                };
                let pargs: Vec<Norm> = pargs.and_then(|l| l.as_coll(CollKind::List).map(|x| x.to_vec())).unwrap_or_default();
                let site = self.site.clone();
                self.next += 1;
                let id = self.next;
                let (sh, w) = (Arc::clone(&self.shared), Arc::clone(&self.wake));
                let ret2 = args.last().and_then(as_name);
                // Quote the cost first; the deploy itself waits for the user.
                self.job(move |b| {
                    let r = b.payer.payer().and_then(|(k, addr)| {
                        let term = b.render_by_hash(&h, &pargs)?;
                        let cost = b.estimate(&term, &k)?;
                        Ok((term, cost, addr))
                    });
                    match r {
                        Ok((term, cost, addr)) => {
                            let bal = b.payer.balance().map(|v| format!(" (balance {v})")).unwrap_or_default();
                            if let Ok(mut s) = sh.lock() {
                                s.prompts.push((
                                    Prompt {
                                        id,
                                        text: format!(
                                            "{site} wants to deploy program {} to the shard. {} {cost} phlo, paid from your wallet {}{bal}.",
                                            &hex(&h)[..12],
                                            b.cfg.dialect.cost_phrase(),
                                            short(&addr)
                                        ),
                                    },
                                    Pending::Deploy { term, cost, ret: ret2 },
                                ));
                            }
                            w();
                            None
                        }
                        Err(e) => reply(err3("shard", &e)),
                    }
                });
            }
            "watch" => {
                let (Some(uri), Some(ch)) = (args.get(1).and_then(|u| u.as_str()), args.get(2).and_then(as_name)) else {
                    return;
                };
                if let Ok(mut s) = self.shared.lock() {
                    s.watches.push((uri.to_string(), ch, Vec::new()));
                }
                self.start_watcher();
            }
            "session" => {
                let Some(uri) = args.get(1).and_then(|u| u.as_str()).map(str::to_string) else {
                    return self.now(reply(err3("type", "session")));
                };
                self.next += 1;
                let id = self.next;
                if let Ok(mut s) = self.shared.lock() {
                    s.prompts.push((
                        Prompt {
                            id,
                            text: format!("{} wants to open a session with {uri}", self.site),
                        },
                        Pending::Session {
                            uri,
                            ret: args.last().and_then(as_name),
                        },
                    ));
                }
                (self.wake)();
            }
            // The chain and staking reads, named by argument 1:
            // `chain!("block", HASH, ret)`. One verb, not one per read:
            // `deploy` is taken, the consent prompt is per-*class* anyway, and
            // `read` already dispatches on its argument's shape.
            "chain" => {
                // The dialect guard is first, so an f1r3fly node never sees a
                // request -- the same shape the `proof`/`replayed` rungs use.
                if self.bridge.cfg.dialect != NodeDialect::Rchain {
                    return self.now(reply(err3(
                        "unavailable",
                        "the chain and staking reads are the rchain dialect's; f1r3fly has no such route",
                    )));
                }
                let sub = args.get(1).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let arg = args.get(2).and_then(|v| v.as_str()).map(str::to_string);
                self.job(move |b| {
                    let r: Result<Norm, String> = match (sub.as_str(), arg.as_deref()) {
                        ("block", Some(h)) => b.block(h).map(|(r, v)| ok3(r.name(), chain::block_to_norm(&v))),
                        ("txns", Some(h)) => b.transactions(h).map(|(r, v)| ok3(r.name(), history::transfers_to_norm(&v))),
                        ("blocks", _) => {
                            let spec = match arg.as_deref().and_then(|a| a.parse::<i32>().ok()) {
                                Some(n) => Blocks::Depth(n.clamp(1, chain::PAGE_MAX_DEPTH)),
                                None => Blocks::Head,
                            };
                            b.blocks(spec).map(|(r, v)| ok3(r.name(), chain::blocks_to_norm(&v)))
                        }
                        ("find-deploy", Some(id)) => b.find_deploy(id).map(|(r, v)| ok3(r.name(), chain::light_to_norm(&v))),
                        ("finalized", Some(h)) => b.is_finalized(h).map(|(r, v)| ok3(r.name(), Norm::bool(v))),
                        ("pool", _) => b.pool().map(|(r, v)| ok3(r.name(), chain::pooled_to_norm(&v))),
                        ("caps", _) => b.capabilities().map(|(r, v)| ok3(r.name(), chain::caps_to_norm(&v))),
                        ("shards", _) => b.shards().map(|(r, v)| ok3(r.name(), chain::shards_to_norm(&v))),
                        ("pos", _) => b.pos_status().map(|(r, v)| ok3(r.name(), pos::status_to_norm(&v))),
                        ("delegations", Some(k)) => b.pos_delegations(k).map(|(r, v)| ok3(r.name(), pos::positions_to_norm(&v))),
                        ("bonds", _) => b.pos_bonds().map(|(r, v)| ok3(r.name(), v)),
                        ("validators", _) => b.pos_active_validators().map(|(r, v)| ok3(r.name(), v)),
                        ("trusted", _) => b.pos_trusted().map(|(r, v)| ok3(r.name(), v)),
                        ("bonds-delegations", Some(k)) => b.pos_delegations_native(k).map(|(r, v)| ok3(r.name(), v)),
                        // A hash-addressed read with no hash is a shape error,
                        // not a node error.
                        ("block" | "txns" | "find-deploy" | "finalized" | "delegations", None) => {
                            return reply(err3("type", "this chain read wants a hash or a key"));
                        }
                        (other, _) => return reply(err3("type", &format!("unknown chain read {other}"))),
                    };
                    reply(match r {
                        Ok(v) => v,
                        Err(e) => err3("shard", &e),
                    })
                });
            }
            // Change the payer's own stake: `stake!("bond", AMOUNT, ret)` or
            // `stake!("withdraw", ret)`. A page names an *amount*, never a key:
            // the validator is the payer's, derived by the node from the
            // `rho:rchain:deployerId` capability the term passes. So this can
            // lock the payer's REV or stage an unbond, and nothing else — it
            // cannot send funds anywhere or touch another key.
            "stake" => {
                if self.bridge.cfg.dialect != NodeDialect::Rchain {
                    return self.now(reply(err3(
                        "unavailable",
                        "the staking writes are the rchain dialect's; f1r3fly has no such route",
                    )));
                }
                let sub = args.get(1).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let amount = args.get(2).and_then(|v| v.as_int());
                self.job(move |b| {
                    let d = match sub.as_str() {
                        "bond" => match amount {
                            Some(a) => b.pos_bond(a),
                            None => return reply(err3("type", "stake bond wants an amount")),
                        },
                        "withdraw" => b.pos_withdraw(),
                        other => return reply(err3("type", &format!("unknown stake action {other}"))),
                    };
                    let d = match d {
                        Ok(d) => d,
                        Err(e) => return reply(err3("shard", &e)),
                    };
                    let id = d.id();
                    reply(match b.pos_settle(&id) {
                        Ok(Ok(())) => ok3("node", Norm::map(vec![(Norm::str("deploy"), Norm::str(&id))])),
                        // A refusal is the *chain's answer*, not a failure --
                        // the deploy succeeded and the program declined -- so it
                        // gets its own code and a page can tell them apart.
                        Ok(Err(reason)) => err3("refused", &reason),
                        Err(e) => err3("shard", &e),
                    })
                });
            }
            // Stake on an operator **the page names**, and take it back. The
            // key is the page's, which is why this is its own consent class: a
            // site trusted to bond or unbond the payer's own stake is not
            // thereby trusted to nominate who holds it. The key is validated
            // before a term is built, and the delegator is always the payer.
            "delegate" | "undelegate" => {
                if self.bridge.cfg.dialect != NodeDialect::Rchain {
                    return self.now(reply(err3(
                        "unavailable",
                        "the staking writes are the rchain dialect's; f1r3fly has no such route",
                    )));
                }
                let undelegate = verb == "undelegate";
                let operator = args.get(1).and_then(key_arg);
                let amount = args.get(2).and_then(|v| v.as_int());
                self.job(move |b| {
                    let Some(operator) = operator else {
                        return reply(err3("type", "this wants a 65-byte operator key"));
                    };
                    let d = if undelegate {
                        b.pos_undelegate(&operator)
                    } else {
                        match amount {
                            Some(a) => b.pos_delegate(&operator, a),
                            None => return reply(err3("type", "delegate wants an amount")),
                        }
                    };
                    let d = match d {
                        Ok(d) => d,
                        Err(e) => return reply(err3("shard", &e)),
                    };
                    let id = d.id();
                    reply(match b.pos_settle(&id) {
                        Ok(Ok(())) => ok3("node", Norm::map(vec![(Norm::str("deploy"), Norm::str(&id))])),
                        // The chain's answer, not a failure -- same code the
                        // `stake` verb uses.
                        Ok(Err(reason)) => err3("refused", &reason),
                        Err(e) => err3("shard", &e),
                    })
                });
            }
            "proof" | "replayed" => self.now(reply(err3(
                "unavailable",
                "the proof and replayed rungs need node work package N1 and the rspace adapter",
            ))),
            _ => self.now(reply(err3("verb", &verb))),
        }
    }

    fn now(&self, o: Option<ShardOut>) {
        if let Some(o) = o {
            Self::push(&self.shared, &self.wake, o);
        }
    }

    /// A send on a session name becomes a deploy, paid for by the wallet and
    /// delivered to the service as `svc!("msg", session, args...)`, where
    /// `session` is the session key's public key (it identifies the session;
    /// the wallet signs).
    fn session_send(&mut self, label: &str, args: &[Norm]) {
        let s = match self.shared.lock() {
            Ok(g) => g.sessions.get(label).map(|s| (s.key.clone(), s.uri.clone())),
            Err(_) => None,
        };
        let Some((key, uri)) = s else { return };
        let args = args.to_vec();
        let ret = args.last().and_then(as_name);
        self.job(move |b| {
            let payload: Vec<Norm> = args.iter().filter(|a| as_name(a).is_none()).cloned().collect();
            if !payload.iter().all(crate::term::portable) {
                return ret.map(|chan| ShardOut::Reply { chan, datum: err3("type", "session payload") });
            }
            let shown: Vec<String> = payload.iter().map(k1ndl1ng_norm::show).collect();
            let term = format!(
                "new lookup(`rho:registry:lookup`), ch in {{ lookup!(`{uri}`, *ch) | for (svc <- ch) {{ svc!(\"msg\", \"{}\"{}{}) }} }}",
                hex(&public_key(&key)),
                if shown.is_empty() { "" } else { ", " },
                shown.join(", ")
            );
            let r = b.payer.payer().and_then(|(pk, _)| b.sign_and_deploy(&pk, &term, 250_000));
            ret.map(|chan| ShardOut::Reply {
                chan,
                datum: match r {
                    Ok(d) => ok3("node", Norm::map(vec![(Norm::str("deploy"), Norm::str(&d.id()))])),
                    Err(e) => err3("shard", &e),
                },
            })
        });
    }

    fn start_watcher(&mut self) {
        if self.watcher {
            return;
        }
        self.watcher = true;
        let (b, sh, w) = (Arc::clone(&self.bridge), Arc::downgrade(&self.shared), Arc::clone(&self.wake));
        let rx = b.events.as_ref().map(|e| e.subscribe());
        let _ = std::thread::Builder::new().name("gaze-shard-watch".into()).spawn(move || {
            loop {
                let Some(shared) = sh.upgrade() else { return }; // tab closed
                let watches: Vec<(String, Name, Vec<u8>)> = shared.lock().map(|s| s.watches.clone()).unwrap_or_default();
                for (i, (uri, ch, last)) in watches.into_iter().enumerate() {
                    if let Ok((r, v)) = b.lookup(&uri) {
                        if v.encode() != last.as_slice() {
                            if let Ok(mut s) = shared.lock() {
                                if let Some(wt) = s.watches.get_mut(i) {
                                    wt.2 = v.encode().to_vec();
                                }
                                s.out.push(ShardOut::Reply {
                                    chan: ch,
                                    datum: Norm::tuple(vec![Norm::str("changed"), Norm::str(r.name()), v]),
                                });
                            }
                            w();
                        }
                    }
                }
                drop(shared);
                // Re-resolve on each finalized block; poll if there is no stream.
                match &rx {
                    Some(r) => {
                        let _ = r.recv_timeout(Duration::from_secs(30));
                    }
                    None => std::thread::sleep(Duration::from_secs(10)),
                }
            }
        });
    }

    /// Prompts waiting for the user.
    pub fn prompts(&self) -> Vec<Prompt> {
        self.shared.lock().map(|s| s.prompts.iter().map(|(p, _)| p.clone()).collect()).unwrap_or_default()
    }

    /// The user's answer to a prompt.
    pub fn answer(&mut self, id: u64, yes: bool) {
        let pending = match self.shared.lock() {
            Ok(mut s) => {
                let i = s.prompts.iter().position(|(p, _)| p.id == id);
                i.map(|i| s.prompts.remove(i).1)
            }
            Err(_) => None,
        };
        let Some(p) = pending else { return };
        match p {
            Pending::Deploy { ret, .. } | Pending::Session { ret, .. } if !yes => {
                self.now(ret.map(|chan| ShardOut::Reply { chan, datum: err3("denied", "the user declined") }))
            }
            Pending::Deploy { term, cost, ret } => {
                let site = self.site.clone();
                let (sh, w) = (Arc::clone(&self.shared), Arc::clone(&self.wake));
                self.job(move |b| {
                    let limit = (cost as i64).saturating_mul(3) / 2 + 10_000;
                    let _ = &site;
                    let d = match b.payer.payer().and_then(|(k, _)| b.sign_and_deploy(&k, &term, limit)) {
                        Ok(d) => d,
                        Err(e) => return ret.map(|chan| ShardOut::Reply { chan, datum: err3("shard", &e) }),
                    };
                    let id = d.id();
                    if let Some(chan) = ret.clone() {
                        Self::push(
                            &sh,
                            &w,
                            ShardOut::Reply {
                                chan,
                                datum: ok3("node", Norm::map(vec![(Norm::str("deploy"), Norm::str(&id))])),
                            },
                        );
                    }
                    // Then once more with the outcome.
                    for _ in 0..100 {
                        std::thread::sleep(Duration::from_secs(3));
                        if let Ok((st, blk)) = b.finalization(&id) {
                            if st != "Pending" {
                                return ret.map(|chan| ShardOut::Reply {
                                    chan,
                                    datum: ok3(
                                        "node",
                                        Norm::map(vec![
                                            (Norm::str("deploy"), Norm::str(&id)),
                                            (Norm::str("status"), Norm::str(&st)),
                                            (Norm::str("block"), blk.map(|h| Norm::str(&h)).unwrap_or_else(Norm::nil)),
                                        ]),
                                    ),
                                });
                            }
                        }
                    }
                    None
                });
            }
            Pending::Session { uri, ret } => {
                self.next += 1;
                let label = format!("rho:gaze:shard/session/{}", self.next);
                let sh = Arc::clone(&self.shared);
                self.job(move |b| {
                    let r = fresh_key().and_then(|key| {
                        let term = format!(
                            "new lookup(`rho:registry:lookup`), ch in {{ lookup!(`{uri}`, *ch) | for (svc <- ch) {{ svc!(\"open\", \"{}\") }} }}",
                            hex(&public_key(&key))
                        );
                        let (pk, _) = b.payer.payer()?;
                        b.sign_and_deploy(&pk, &term, 250_000)?;
                        Ok(key)
                    });
                    let chan = ret?;
                    match r {
                        Ok(key) => {
                            if let Ok(mut s) = sh.lock() {
                                s.sessions.insert(label.clone(), Session { key, uri });
                            }
                            Some(ShardOut::BindSession { chan, label })
                        }
                        Err(e) => Some(ShardOut::Reply { chan, datum: err3("shard", &e) }),
                    }
                });
            }
        }
    }

    /// Everything ready for the tab.
    pub fn drain(&mut self) -> Vec<ShardOut> {
        self.shared.lock().map(|mut s| std::mem::take(&mut s.out)).unwrap_or_default()
    }

    /// Close a session: its key is destroyed.
    pub fn close_session(&mut self, label: &str) {
        if let Ok(mut s) = self.shared.lock() {
            s.sessions.remove(label);
        }
    }
}

// ---------------------------------------------------------------------------

/// Small blobs in F1R3Drive's on-chain layout (spec §9.7): a metadata map
/// `{"type": "f", "firstChunk": bytes, "otherChunks": {i: path}}` on the
/// public channel `@"<root><hex>"`, further chunks on their own channels.
/// Read by exploratory deploys that peek without consuming.
pub struct DriveSource {
    pub bridge: Arc<Bridge>,
    pub root: String,
}

pub const DRIVE_MAX: usize = 256 * 1024;

fn peek_term(path: &str) -> String {
    let p = path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("new return in {{ for (@v <<- @\"{p}\") {{ return!(v) }} }}")
}

fn bytes_of(n: &Norm) -> Option<Vec<u8>> {
    match n.as_lit() {
        Some(Lit::Bytes(b)) => Some(b.to_vec()),
        _ => n.as_coll(CollKind::List).and_then(|l| {
            let mut out = Vec::new();
            for x in l {
                out.extend(bytes_of(x)?);
            }
            Some(out)
        }),
    }
}

impl BlobSource for DriveSource {
    fn name(&self) -> &str {
        "shard"
    }
    fn get(&self, h: &[u8; 32]) -> Result<Option<Vec<u8>>, String> {
        let path = format!("{}{}", self.root, hex(h));
        let (_, meta) = self.bridge.explore(&peek_term(&path))?;
        if meta.is_nil() {
            return Ok(None);
        }
        let mut out = meta.map_get("firstChunk").and_then(bytes_of).unwrap_or_default();
        let mut others: Vec<(i64, String)> = meta
            .map_get("otherChunks")
            .and_then(|m| m.as_coll(CollKind::Map))
            .map(|kv| {
                kv.chunks(2)
                    .filter_map(|p| {
                        let i = p[0].as_int().or_else(|| p[0].as_str().and_then(|s| s.parse().ok()))?;
                        Some((i, p[1].as_str()?.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        others.sort();
        for (_, p) in others {
            if out.len() > DRIVE_MAX {
                return Err("on-chain blob exceeds 256 KiB".into());
            }
            let (_, c) = self.bridge.explore(&peek_term(&p))?;
            out.extend(bytes_of(&c).ok_or("chunk is not bytes")?);
        }
        Ok(Some(out))
    }
}

/// `1111abcd…wxyz` for prompts.
pub fn short(addr: &str) -> String {
    if addr.len() <= 14 {
        addr.to_string()
    } else {
        format!("{}…{}", &addr[..8], &addr[addr.len() - 6..])
    }
}
