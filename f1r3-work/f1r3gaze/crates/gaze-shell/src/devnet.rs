//! The local devnet: one `rnode`, configured and supervised.
//!
//! Everything here is *launcher* work. The node already implements what the devnet exists to exercise
//! — a two-shard **gateway** that coordinates a cross-shard transaction (`POST /api/v1/txn` on the
//! admin listener), the **ERTP** object API, and the **OCapN** peer listeners — but only as config, a
//! branch, and tests. This writes the config and starts the process; a node is never modified.
//!
//! A node reads `<data-dir>/rnode.conf` by default (`configuration.rs`), so `up` writes there and
//! spawns `rnode run --data-dir <data>`. Which *build* is run is a flag, because ERTP and OCapN live on
//! the `ocapn-ertp` branch rather than on `dev`: the launcher does not care, it just starts whatever
//! binary it is pointed at.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gaze_net::Http;
use gaze_shard::{Node, NodeDialect};

/// The throwaway dev key the node's own harness and the Endo spike both use.
pub const DEV_KEY: &str = "a68a6e6cca30f81bd24a719f3145d20e8424bd7b396309b0708a16c7d8000b76";
/// The REV address that key derives to. Funded at genesis, so deploys — and the OCapN bridge's own
/// deploys, which the node signs with its deployer key — have phlo to spend.
pub const DEV_ADDRESS: &str = "11112VYAt8rUGNRRZX3eJdgagaAhtWTK8Js7F7X5iqddMVqyDTtYau";
pub const DEV_BALANCE: &str = "1000000000000";
/// The validator's bond. Above the shard's minimum (1); the size only matters for the active-set
/// draw, and a devnet has one key to draw.
pub const DEV_STAKE: &str = "1000000";

/// Which OCapN listener to bind, if any. Each is off until it names a bind address, and each needs an
/// rnode built from the `ocapn-ertp` branch.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Ocapn {
    None,
    /// The transport `@endo/ocapn` speaks, and the one the branch's interop run is recorded over.
    Websocket,
    /// The transport a *remote* peer should use — its handshake is verified against Agoric's own.
    Noise,
}

impl Ocapn {
    pub fn parse(s: &str) -> Option<Ocapn> {
        match s {
            "none" => Some(Ocapn::None),
            "websocket" | "ws" => Some(Ocapn::Websocket),
            "noise" => Some(Ocapn::Noise),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Ocapn::None => "none",
            Ocapn::Websocket => "websocket",
            Ocapn::Noise => "noise",
        }
    }
}

pub struct Options {
    /// How many shards the node is a member of. Two or more makes it a **gateway**, which is what
    /// lets it coordinate a cross-shard transaction itself.
    pub shards: u32,
    pub rnode: PathBuf,
    pub data: PathBuf,
    pub ocapn: Ocapn,
    /// Added to every port, so a second devnet can run beside the first.
    pub port_base: u16,
    pub fresh: bool,
}

pub struct Devnet {
    pub data: PathBuf,
    pub http_port: u16,
    pub admin_port: u16,
    pub ocapn_port: u16,
}

impl Devnet {
    pub fn http_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.http_port)
    }
    pub fn admin_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.admin_port)
    }
    fn pid_file(&self) -> PathBuf {
        self.data.join("devnet.pid")
    }
}

/// The node's ports, offset by `base`. The protocol and discovery ports are included because a second
/// devnet on the same host would otherwise collide on them even though it serves no peers.
fn ports(base: u16) -> (u16, u16, u16, u16, u16, u16, u16) {
    (
        40400 + base, // protocol-server
        40404 + base, // discovery
        40401 + base, // grpc-external
        40402 + base, // grpc-internal
        40403 + base, // http
        40405 + base, // admin http
        22060 + base, // ocapn
    )
}

/// The HOCON the node reads from `<data>/rnode.conf`.
fn config(opts: &Options) -> String {
    let (proto, _discovery, ge, gi, http, admin, ocapn) = ports(opts.port_base);
    let shards: Vec<String> = (0..opts.shards.max(1))
        .map(|i| {
            if i == 0 {
                "{ shard-name = root, parent-shard-id = / }".to_string()
            } else {
                // A child of the root shard: `/root/child1`, `/root/child2`, …
                format!("{{ shard-name = child{i}, parent-shard-id = /root }}")
            }
        })
        .collect();

    let mut c = String::new();
    c.push_str("// Written by `f1r3gaze devnet up` — regenerated on every `up`.\n");
    c.push_str("standalone = true\n");
    c.push_str("dev-mode = true\n");
    // **Autopropose, as the node's own devnet does** (`tools/devnet.sh`, `docs/src/node/devnet.md`).
    // Without it a block comes only from a deploy, so anything that waits on one — a deploy's
    // status, a gateway leg's reply, a bridged OCapN call — is waiting on deploy timing rather than
    // on the chain. The cost is that a devnet left up grows ~1,800 blocks an hour, which is why
    // `down` matters.
    c.push_str("autopropose = true\n");
    c.push_str("propose-on-deploy = true\n\n");
    c.push_str(&format!(
        "protocol-server {{\n  host = \"127.0.0.1\"\n  port = {proto}\n  no-upnp = true\n}}\n\n"
    ));
    c.push_str(&format!(
        "api-server {{\n  host = \"127.0.0.1\"\n  port-grpc-external = {ge}\n  port-grpc-internal = {gi}\n  \
         port-http = {http}\n  port-admin-http = {admin}\n"
    ));
    if opts.shards >= 2 {
        // The gateway's transaction route is gated on this as well as on having a gateway.
        c.push_str("  enable-txn-api = true\n");
    }
    match opts.ocapn {
        Ocapn::None => {}
        Ocapn::Websocket => c.push_str(&format!("  ocapn-listen-websocket = \"127.0.0.1:{ocapn}\"\n")),
        Ocapn::Noise => c.push_str(&format!("  ocapn-listen-noise = \"127.0.0.1:{ocapn}\"\n")),
    }
    c.push_str("}\n\n");
    c.push_str(&format!(
        "casper {{\n  // Each shard reaches genesis under its own id: /root, /root/child1, …\n  \
         shards = [\n    {}\n  ]\n  validator-private-key = \"{DEV_KEY}\"\n}}\n\n",
        shards.join(",\n    ")
    ));
    c.push_str(&format!("dev {{\n  deployer-private-key = \"{DEV_KEY}\"\n}}\n"));
    c
}

/// Bring the devnet up and wait until every member shard has a chain.
pub fn up(opts: &Options) -> Result<Devnet, String> {
    if !opts.rnode.exists() {
        return Err(format!(
            "no rnode at {} — build it, or pass --rnode",
            opts.rnode.display()
        ));
    }
    let (_, _, _, _, http_port, admin_port, ocapn_port) = ports(opts.port_base);
    let d = Devnet {
        data: opts.data.clone(),
        http_port,
        admin_port,
        ocapn_port,
    };

    if opts.fresh && d.data.exists() {
        std::fs::remove_dir_all(&d.data).map_err(|e| format!("{}: {e}", d.data.display()))?;
    }
    std::fs::create_dir_all(d.data.join("genesis")).map_err(|e| e.to_string())?;
    std::fs::write(d.data.join("rnode.conf"), config(opts)).map_err(|e| e.to_string())?;
    // The genesis wallet is written only when absent: a devnet that is already funded keeps its chain,
    // and the file is what genesis is derived from.
    let wallets = d.data.join("genesis/wallets.txt");
    if !wallets.exists() {
        std::fs::write(&wallets, format!("{DEV_ADDRESS},{DEV_BALANCE}\n")).map_err(|e| e.to_string())?;
    }
    // **And the bond, or the validator is not in the active set.** With `wallets.txt` alone the node
    // bonds a *random* key set of its own, and the validator it was told to run as is refused with
    // "[pos] not proposing: … is not in the active set", so the chain stops at genesis and every
    // deploy — including a cross-shard leg — waits for a block that never comes. The two files have
    // to name the same key, which is what deriving the public key here is for.
    let bonds = d.data.join("genesis/bonds.txt");
    if !bonds.exists() {
        let pubkey = gaze_shard::deploy::public_key_hex(DEV_KEY)?;
        std::fs::write(&bonds, format!("{pubkey} {DEV_STAKE}\n")).map_err(|e| e.to_string())?;
    }

    let log = std::fs::File::create(d.data.join("rnode.log")).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&opts.rnode);
    cmd.arg("run").arg("--data-dir").arg(&d.data);
    if opts.ocapn != Ocapn::None {
        // Created on first use, 0600. Passed on the command line so the config file holds no
        // machine-specific path — the same split the branch's own spike uses.
        cmd.arg("--ocapn-identity-key").arg(d.data.join("ocapn-identity.key"));
    }
    let child = cmd
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| format!("spawning {}: {e}", opts.rnode.display()))?;
    std::fs::write(d.pid_file(), child.id().to_string()).map_err(|e| e.to_string())?;

    println!(
        "rnode {} (pid {}) — {} shard(s), ocapn {}",
        opts.rnode.display(),
        child.id(),
        opts.shards.max(1),
        opts.ocapn.name()
    );
    println!("  api    {}", d.http_base());
    println!("  admin  {}", d.admin_base());
    if opts.ocapn != Ocapn::None {
        println!("  ocapn  {} @ 127.0.0.1:{}", opts.ocapn.name(), d.ocapn_port);
    }
    println!("  log    {}", d.data.join("rnode.log").display());

    wait_ready(&d, Duration::from_secs(120))?;
    Ok(d)
}

/// Every member shard has a chain.
///
/// `/api/v1/shards` answering is **not** readiness: the HTTP surface is up before the shards'
/// genesis ceremonies finish, so a shard reports a height only once it has one. This is the branch's
/// own rule (`node/tests/gateway.rs`), and it is the difference between "the node is listening" and
/// "a deploy will land".
fn wait_ready(d: &Devnet, timeout: Duration) -> Result<(), String> {
    let node = Node::new(&d.http_base(), NodeDialect::Rchain, Http::new());
    let t0 = Instant::now();
    let mut last = String::new();
    while t0.elapsed() < timeout {
        match node.shards() {
            Ok(s) if !s.shards.is_empty() && s.shards.iter().all(|x| x.latest_block_number >= 1) => {
                println!("ready: {} shard(s), primary {}", s.shard_count, s.primary_shard);
                return Ok(());
            }
            Ok(s) => last = format!("{} of {} shard(s) have a chain", s.shards.iter().filter(|x| x.latest_block_number >= 1).count(), s.shard_count),
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("the devnet did not become ready in {timeout:?}: {last}"))
}

/// What is up, read from the node itself.
pub fn status(data: &Path, port_base: u16) -> Result<(), String> {
    let (_, _, _, _, http_port, admin_port, _) = ports(port_base);
    let pid = std::fs::read_to_string(data.join("devnet.pid")).ok().and_then(|s| s.trim().parse::<i32>().ok());
    match pid {
        Some(p) if alive(p) => println!("rnode pid {p} is running"),
        Some(p) => println!("rnode pid {p} is recorded but not running"),
        None => println!("no devnet recorded at {}", data.display()),
    }
    let node = Node::new(&format!("http://127.0.0.1:{http_port}"), NodeDialect::Rchain, Http::new());
    match node.shards() {
        Ok(s) => {
            println!("api   http://127.0.0.1:{http_port}");
            println!("admin http://127.0.0.1:{admin_port}");
            println!("primary {}", s.primary_shard);
            for x in &s.shards {
                println!("  {}  {}  #{}", x.shard_id, if x.primary { "primary" } else { "member" }, x.latest_block_number);
            }
            Ok(())
        }
        Err(e) => Err(format!("the node did not answer: {e}")),
    }
}

/// Stop the devnet. `--purge` also removes its data dir, and with it the chain.
pub fn down(data: &Path, purge: bool) -> Result<(), String> {
    if let Some(p) = std::fs::read_to_string(data.join("devnet.pid")).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
        if alive(p) {
            let _ = Command::new("kill").arg(p.to_string()).status();
            // Give it a moment to close its stores rather than pulling them from under it.
            for _ in 0..40 {
                if !alive(p) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            println!("stopped pid {p}{}", if alive(p) { " (still running; it may be flushing)" } else { "" });
        }
        let _ = std::fs::remove_file(data.join("devnet.pid"));
    }
    if purge {
        std::fs::remove_dir_all(data).map_err(|e| format!("{}: {e}", data.display()))?;
        println!("purged {}", data.display());
    } else {
        println!("kept {} — the chain is there for the next `up`", data.display());
    }
    Ok(())
}

fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}
