//! `f1r3gaze` — the browser.
//!
//! ```text
//! f1r3gaze [URL]                     open a window
//! f1r3gaze --headless URL [--allow] [--click SELECTOR]... [--timeout SECS]
//!          [--wait SECS] [--log FILE.gzlog]
//!                                    run a page without a window and print
//!                                    its committed document and console
//! f1r3gaze --profile DIR ...         use DIR as the profile
//! f1r3gaze wallet list|new [LABEL]|import FILE [LABEL]|export ADDRESS [FILE]
//!                 |use ADDRESS|remove ADDRESS|balance [ADDRESS]
//!                 |send TO AMOUNT [DESCRIPTION]|faucet [ADDRESS]
//!                                    manage the wallets that pay for deploys
//! f1r3gaze chain block HASH | blocks [N] | find-deploy ID | finalized HASH
//!                | pool | caps | shards        [--json]
//! f1r3gaze pos status | delegations KEY | bonds | validators | trusted
//!                                              [--json]
//! f1r3gaze pos bond AMOUNT | unbond
//! f1r3gaze pos delegate OPERATOR AMOUNT | undelegate OPERATOR
//!                                    read the chain and its staking state, and
//!                                    change the payer's stake -- on itself, or
//!                                    on an operator it names (the rchain dialect
//!                                    only; bond is permissioned, so an
//!                                    unadmitted key is refused)
//! f1r3gaze --version
//! ```

use gaze_shell::{Engine, headless, profile};
use std::time::Duration;

fn usage() -> ! {
    eprintln!(
        "usage: f1r3gaze [--profile DIR] [URL]\n       f1r3gaze [--profile DIR] --headless URL [--allow] [--click SELECTOR]... [--timeout SECS] [--log FILE]"
    );
    std::process::exit(2)
}

fn shortn(s: &str) -> String {
    gaze_shard::bridge::short(s)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn print_json(v: serde_json::Value) -> Result<(), String> {
    println!("{}", serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?);
    Ok(())
}

/// Split off a trailing `--json`, and refuse on any dialect but rchain before
/// a request is made.
fn read_args<'a>(eng: &gaze_shell::Engine, args: &'a [String], what: &str) -> Result<(Vec<&'a str>, bool), String> {
    if eng.bridge.cfg.dialect != gaze_shard::NodeDialect::Rchain {
        return Err(format!("the {what} are the rchain dialect's; f1r3fly has no such route"));
    }
    let json = args.iter().any(|a| a == "--json");
    Ok((args.iter().map(String::as_str).filter(|x| *x != "--json").collect(), json))
}

fn chain(eng: &gaze_shell::Engine, args: &[String]) -> Result<(), String> {
    use gaze_shard::chain::Blocks;
    let (a, json) = read_args(eng, args, "chain reads")?;
    let b = &eng.bridge;
    let need = |i: usize, of: &str| a.get(i).copied().ok_or_else(|| format!("chain {} needs {of}", a.first().copied().unwrap_or("")));
    match a.first().copied().unwrap_or("caps") {
        "block" => {
            let bi = b.block(need(1, "a block hash")?)?.1;
            if json {
                return print_json(serde_json::to_value(&bi).map_err(|e| e.to_string())?);
            }
            let i = &bi.block_info;
            println!("{} #{}  {} deploys  sender {}  {}", shortn(&i.block_hash), i.block_number, i.deploy_count, shortn(&i.sender), i.timestamp);
            for d in &bi.deploys {
                println!("  {}  {}  {}{}", shortn(&d.sig), shortn(&d.deployer), if d.errored { "ERROR " } else { "" }, clip(&d.term, 60));
            }
        }
        "blocks" => {
            let spec = match a.get(1) {
                Some(n) => Blocks::Depth(n.parse::<i32>().map_err(|_| "chain blocks wants a depth")?),
                None => Blocks::Head,
            };
            let bs = b.blocks(spec)?.1;
            if json {
                return print_json(serde_json::to_value(&bs).map_err(|e| e.to_string())?);
            }
            for i in &bs {
                println!("{} #{}  {} deploys  {}", shortn(&i.block_hash), i.block_number, i.deploy_count, i.timestamp);
            }
        }
        "find-deploy" => {
            let i = b.find_deploy(need(1, "a deploy id")?)?.1;
            if json {
                return print_json(serde_json::to_value(&i).map_err(|e| e.to_string())?);
            }
            println!("{} #{}", shortn(&i.block_hash), i.block_number);
        }
        "finalized" => println!("{}", b.is_finalized(need(1, "a block hash")?)?.1),
        "pool" => {
            let ps = b.pool()?.1;
            if json {
                return print_json(serde_json::to_value(&ps).map_err(|e| e.to_string())?);
            }
            for p in &ps {
                println!("{}  {}  {}  {}", shortn(&p.deploy_id), shortn(&p.deployer), p.phlo_limit, clip(&p.term, 60));
            }
        }
        "caps" => {
            let c = b.capabilities()?.1;
            if json {
                return print_json(serde_json::to_value(&c).map_err(|e| e.to_string())?);
            }
            println!(
                "autopropose      {}\nproposeOnDeploy  {}\nmanualPropose    {}\nadminHttp        {}\ndevMode          {}\nfaucet           {}",
                c.autopropose, c.propose_on_deploy, c.manual_propose, c.admin_http, c.dev_mode, c.faucet
            );
        }
        "shards" => {
            let s = b.shards()?.1;
            if json {
                return print_json(serde_json::to_value(&s).map_err(|e| e.to_string())?);
            }
            println!("primary {}", s.primary_shard);
            for x in &s.shards {
                println!("{}  {}  #{}", x.shard_id, if x.primary { "primary" } else { "member" }, x.latest_block_number);
            }
        }
        other => return Err(format!("unknown chain read {other}")),
    }
    Ok(())
}

fn pos(eng: &gaze_shell::Engine, args: &[String]) -> Result<(), String> {
    let (a, json) = read_args(eng, args, "staking reads")?;
    let b = &eng.bridge;
    match a.first().copied().unwrap_or("status") {
        "status" => {
            let s = b.pos_status()?.1;
            if json {
                return print_json(serde_json::to_value(&s).map_err(|e| e.to_string())?);
            }
            println!(
                "epoch {}  length {}  quarantine {}  {} blocks to the boundary  head #{}",
                s.epoch, s.epoch_length, s.quarantine_length, s.blocks_until_epoch_boundary, s.latest_block_number
            );
            println!("active validators: {}", s.active_validators.len());
            for w in &s.pending_withdrawals {
                println!("  withdrawal {}  deadline {}  in {}", shortn(&w.validator), w.deadline, w.blocks_remaining);
            }
        }
        "delegations" => {
            let k = a.get(1).copied().ok_or("pos delegations needs a validator key")?;
            let ps = b.pos_delegations(k)?.1;
            if json {
                return print_json(serde_json::to_value(&ps).map_err(|e| e.to_string())?);
            }
            if ps.is_empty() {
                println!("no positions");
            }
            for p in &ps {
                let staged = match &p.pending_undelegation {
                    Some(u) => format!("deadline {} in {}", u.deadline, u.blocks_remaining),
                    None => "none".into(),
                };
                println!("{}  amount {}  accrued {}  staged: {staged}", shortn(&p.operator), p.amount, p.accrued_rewards);
            }
        }
        // The native reads reply a term, so they are shown as one.
        "bonds" => println!("{}", k1ndl1ng_norm::show(&b.pos_bonds()?.1)),
        "validators" => println!("{}", k1ndl1ng_norm::show(&b.pos_active_validators()?.1)),
        "trusted" => println!("{}", k1ndl1ng_norm::show(&b.pos_trusted()?.1)),
        // Writes. `bond` is permissioned: a key that is not in the shard's
        // trusted set is refused until a stakeholder admits it, and the node's
        // own words are what gets printed.
        "bond" => {
            let amount: i64 = a
                .get(1)
                .copied()
                .ok_or("pos bond needs an amount, in drops")?
                .parse()
                .map_err(|_| "the amount must be a whole number of drops")?;
            let d = b.pos_bond(amount)?;
            println!("deploy {}", d.id());
            match b.pos_settle(&d.id())? {
                Ok(()) => println!("bonded {amount} drops"),
                Err(reason) => println!("refused: {reason}"),
            }
        }
        "unbond" => {
            let d = b.pos_withdraw()?;
            println!("deploy {}", d.id());
            match b.pos_settle(&d.id())? {
                Ok(()) => println!("withdrawal staged: it pays after the quarantine at the next boundary"),
                Err(reason) => println!("refused: {reason}"),
            }
        }
        // These two **name an operator key**; the delegator is always the
        // payer. The key is validated before a term is built.
        "delegate" => {
            let op = a.get(1).copied().ok_or("pos delegate needs a 65-byte operator key")?;
            let amount: i64 = a
                .get(2)
                .copied()
                .ok_or("pos delegate needs an amount, in drops")?
                .parse()
                .map_err(|_| "the amount must be a whole number of drops")?;
            let d = b.pos_delegate(op, amount)?;
            println!("deploy {}", d.id());
            match b.pos_settle(&d.id())? {
                Ok(()) => println!("delegated {amount} drops to {}", shortn(op)),
                Err(reason) => println!("refused: {reason}"),
            }
        }
        "undelegate" => {
            let op = a.get(1).copied().ok_or("pos undelegate needs a 65-byte operator key")?;
            let d = b.pos_undelegate(op)?;
            println!("deploy {}", d.id());
            match b.pos_settle(&d.id())? {
                Ok(()) => println!("undelegation staged: it pays after the quarantine at the next boundary"),
                Err(reason) => println!("refused: {reason}"),
            }
        }
        other => return Err(format!("unknown pos read {other}")),
    }
    Ok(())
}

fn wallet(eng: &gaze_shell::Engine, args: &[String]) -> Result<(), String> {
    use gaze_wallet::Address;
    let w = &eng.wallets;
    let arg = |i: usize| args.get(i).map(String::as_str);
    let need = |i: usize, what: &str| args.get(i).cloned().ok_or(format!("wallet {} needs {what}", args[0]));
    let chosen = |i: usize| -> Result<Address, String> {
        match arg(i) {
            Some(a) => Address::parse(a),
            None => w.active().ok_or_else(|| "no active wallet".to_string()),
        }
    };
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" => {
            for (e, active) in w.list() {
                println!("{} {}  {}", if active { "*" } else { " " }, e.address, e.label);
            }
        }
        "new" => println!("{}", w.create(arg(1).unwrap_or(""))?),
        "import" => {
            let f = need(1, "a wallet file")?;
            let text = std::fs::read_to_string(&f).map_err(|e| format!("{f}: {e}"))?;
            println!("{}", w.import(&text, arg(2).unwrap_or(""))?);
        }
        "export" => {
            let a = Address::parse(&need(1, "an address")?)?;
            let body = w.export(&a)?;
            match arg(2) {
                Some(f) => {
                    std::fs::write(f, &body).map_err(|e| format!("{f}: {e}"))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o600));
                    }
                    println!("wrote {f}");
                }
                None => println!("{body}"),
            }
        }
        "use" => w.set_active(&Address::parse(&need(1, "an address")?)?)?,
        "remove" => w.remove(&Address::parse(&need(1, "an address")?)?)?,
        "balance" => {
            let a = chosen(1)?;
            // The rchain dialect reads the balance from the node's native
            // `revVault`; the f1r3fly dialect reads it from Embers, which is
            // also the only one that reports transfer history.
            if eng.bridge.cfg.dialect == gaze_shard::NodeDialect::Rchain {
                let (rung, drops) = eng.bridge.rev_balance(a.as_str())?;
                println!("{a}  {drops} drops ({})", rung.name());
            } else {
                let s = w.state(&a)?;
                println!("{a}  {}", s.balance);
                for t in s.transfers.iter().rev().take(20) {
                    println!("  {}  {} -> {}  {}  {}", t.timestamp, t.from, t.to, t.amount, t.description.as_deref().unwrap_or(""));
                }
            }
        }
        "send" => {
            let from = w.active().ok_or("no active wallet")?;
            let to = Address::parse(&need(1, "a recipient")?)?;
            let amount: i64 = need(2, "an amount")?.parse().map_err(|_| "the amount must be a whole number")?;
            if eng.bridge.cfg.dialect == gaze_shard::NodeDialect::Rchain {
                // Amounts in the smallest unit (1 REV = 10^8 drops), as the
                // node's `revVault` takes them.
                let d = eng.bridge.rev_transfer(to.as_str(), amount)?;
                println!("deploy {}", d.id());
            } else {
                let id = w.transfer(&from, &to, amount, arg(3))?;
                println!("deploy {id}");
            }
        }
        "faucet" => {
            let a = chosen(1)?;
            let (id, drops) = eng.bridge.faucet(a.as_str())?;
            println!("{a}  funded {drops} drops  deploy {id}");
        }
        other => return Err(format!("unknown wallet command {other}")),
    }
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut dir = profile::default_dir();
    let mut url: Option<String> = None;
    let mut is_headless = false;
    let mut opts = headless::Options::default();
    let mut log: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--version" | "-V" => {
                println!("f1r3gaze {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => usage(),
            "--profile" => dir = args.next().unwrap_or_else(|| usage()).into(),
            "--headless" => is_headless = true,
            "--allow" => opts.allow = true,
            "--click" => opts.clicks.push(args.next().unwrap_or_else(|| usage())),
            "--timeout" => opts.timeout = Duration::from_secs(args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage())),
            "--log" => log = Some(args.next().unwrap_or_else(|| usage())),
            "--wait" => opts.wait = Duration::from_secs(args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage())),
            "wallet" | "chain" | "pos" => {
                let which = a.clone();
                let rest: Vec<String> = args.by_ref().collect();
                let eng = Engine::new(dir.clone());
                let r = match which.as_str() {
                    "chain" => chain(&eng, &rest),
                    "pos" => pos(&eng, &rest),
                    _ => wallet(&eng, &rest),
                };
                if let Err(e) = r {
                    eprintln!("f1r3gaze: {e}");
                    std::process::exit(1);
                }
                return;
            }
            s if s.starts_with("--") => usage(),
            s => url = Some(s.to_string()),
        }
    }
    let eng = Engine::new(dir);
    if is_headless {
        let url = url.unwrap_or_else(|| usage());
        let r = headless::run(eng, &url, &opts);
        println!("url:    {}", r.url);
        println!("stage:  {:?}", r.stage);
        println!("title:  {}", r.title);
        if let Some(n) = &r.notice {
            println!("notice: {n}");
        }
        for p in &r.prompts {
            println!("prompt: {p} -> {}", if opts.allow { "allowed" } else { "denied" });
        }
        for (lvl, line) in &r.console {
            println!("console[{lvl}]: {line}");
        }
        println!("{}", r.document);
        if let (Some(path), Some(bytes)) = (log, r.log) {
            if let Err(e) = std::fs::write(&path, bytes) {
                eprintln!("could not write {path}: {e}");
            }
        }
        let code = if matches!(r.stage, gaze_shell::tab::Stage::Failed(_)) { 1 } else { 0 };
        std::process::exit(code);
    }
    #[cfg(feature = "window")]
    {
        let home = eng.settings.home.clone();
        if let Err(e) = gaze_shell::chrome::launch(eng, &url.unwrap_or(home)) {
            eprintln!("f1r3gaze: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(feature = "window"))]
    {
        let _ = url;
        eprintln!("this build has no window; use --headless");
        std::process::exit(2);
    }
}
