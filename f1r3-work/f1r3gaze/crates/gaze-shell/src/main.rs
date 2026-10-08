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
            "wallet" => {
                let rest: Vec<String> = args.by_ref().collect();
                let eng = Engine::new(dir.clone());
                if let Err(e) = wallet(&eng, &rest) {
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
