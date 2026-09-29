//! `f1r3c` — the F1R3Gaze toolchain (spec §3).
//!
//! ```text
//! f1r3c compile IN.rho [-o OUT.knf] [--level k0|k1|k1g|k2]
//!               [--import IDENT=URN]... [--semiring S] [--ceiling C]
//!               [--frame-budget N] [--sync-budget N]
//! f1r3c inspect FILE.knf          manifest, hashes and the program text
//! f1r3c integrity FILE.knf        the value for the HTML integrity attribute
//! f1r3c site DIR [--entry index.html] [--mirror URL]... [--out OUT]
//!               package DIR for publishing: every file is written to
//!               OUT/blobs/<hash> (upload to any mirror), and OUT/manifest.rho
//!               holds the site manifest as a term for the registry entry
//!               rho:serve:1:<publisher>:<project>:<version>.
//! ```

use gaze_knf::Knf;
use gaze_net::{digest, hex};
use gaze_shard::SiteManifest;
use k1ndl1ng_parse::Level;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("f1r3c: {msg}");
    std::process::exit(1)
}

fn level(s: &str) -> Level {
    match s.to_ascii_lowercase().as_str() {
        "k0" => Level::K0,
        "k1" => Level::K1,
        "k1g" => Level::K1G,
        "k2" => Level::K2,
        _ => die(format!("unknown level {s}")),
    }
}

fn read(p: &str) -> Vec<u8> {
    std::fs::read(p).unwrap_or_else(|e| die(format!("{p}: {e}")))
}

fn load_knf(p: &str) -> Knf {
    Knf::decode(&read(p)).unwrap_or_else(|e| die(format!("{p}: {e:?}")))
}

fn compile(mut a: std::vec::IntoIter<String>) {
    let input = a.next().unwrap_or_else(|| die("compile needs an input file"));
    let mut out: Option<String> = None;
    let mut lvl = Level::K1G;
    let mut imports: Vec<(String, String)> = Vec::new();
    let (mut semiring, mut ceiling, mut fb, mut sb) = (None, None, None, None);
    while let Some(x) = a.next() {
        let mut val = || a.next().unwrap_or_else(|| die(format!("{x} needs a value")));
        match x.as_str() {
            "-o" => out = Some(val()),
            "--level" => lvl = level(&val()),
            "--import" => {
                let v = val();
                let (i, u) = v.split_once('=').unwrap_or_else(|| die("--import wants IDENT=URN"));
                imports.push((i.into(), u.into()));
            }
            "--semiring" => semiring = Some(val()),
            "--ceiling" => ceiling = Some(val()),
            "--frame-budget" => fb = Some(val().parse().unwrap_or_else(|_| die("budget"))),
            "--sync-budget" => sb = Some(val().parse().unwrap_or_else(|_| die("budget"))),
            other => die(format!("unknown option {other}")),
        }
    }
    let src = String::from_utf8(read(&input)).unwrap_or_else(|_| die("input is not UTF-8"));
    let urns: Vec<(&str, &str)> = imports.iter().map(|(i, u)| (i.as_str(), u.as_str())).collect();
    let mut k = Knf::from_source(&src, lvl, &urns).unwrap_or_else(|e| match e {
        gaze_knf::KnfError::Compile(ds) => die(ds.join("\n")),
        other => die(format!("{other:?}")),
    });
    if let Some(s) = semiring {
        k.manifest.semiring = s;
    }
    if let Some(c) = ceiling {
        k.manifest.ceiling = c;
    }
    if let Some(f) = fb {
        k.manifest.frame_budget = f;
    }
    if let Some(s) = sb {
        k.manifest.sync_budget = s;
    }
    let out = out.unwrap_or_else(|| Path::new(&input).with_extension("knf").to_string_lossy().into_owned());
    std::fs::write(&out, k.encode()).unwrap_or_else(|e| die(format!("{out}: {e}")));
    println!("{out}");
    println!("integrity=\"{}\"", k.integrity());
    for (i, u) in &k.manifest.imports {
        println!("  import {i} = {u}");
    }
}

fn inspect(p: &str) {
    let k = load_knf(p);
    println!("level:      {:?}", k.level);
    println!("program:    {}", k.integrity());
    println!("grant hash: blake2b-256:{}", hex(&k.grant_hash()));
    println!("file hash:  f1r3h://blake2b-256/{}", hex(&digest(&read(p))));
    println!("semiring:   {}  ceiling: {}  fair: {}", k.manifest.semiring, k.manifest.ceiling, k.manifest.fair);
    println!("budget:     frame {}  sync {}", k.manifest.frame_budget, k.manifest.sync_budget);
    for (i, (id, u)) in k.manifest.imports.iter().enumerate() {
        println!("import f{i}: {id} = {u}");
    }
    println!("\n{}", k1ndl1ng_norm::show_pretty(&k.body));
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { die(format!("{}: not a directory", dir.display())) };
    let mut es: Vec<_> = rd.flatten().collect();
    es.sort_by_key(|e| e.file_name());
    for e in es {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        if e.path().is_dir() {
            walk(&e.path(), &r, out);
        } else {
            out.push((r, e.path()));
        }
    }
}

fn site(mut a: std::vec::IntoIter<String>) {
    let dir = a.next().unwrap_or_else(|| die("site needs a directory"));
    let mut entry = "index.html".to_string();
    let mut mirrors = Vec::new();
    let mut out = PathBuf::from(format!("{dir}.site"));
    while let Some(x) = a.next() {
        let mut val = || a.next().unwrap_or_else(|| die(format!("{x} needs a value")));
        match x.as_str() {
            "--entry" => entry = val(),
            "--mirror" => mirrors.push(val()),
            "--out" => out = val().into(),
            other => die(format!("unknown option {other}")),
        }
    }
    let mut files = Vec::new();
    walk(Path::new(&dir), "", &mut files);
    let blobs = out.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap_or_else(|e| die(e));
    let mut map = BTreeMap::new();
    for (name, path) in files {
        let b = std::fs::read(&path).unwrap_or_else(|e| die(e));
        if name.ends_with(".knf") {
            Knf::decode(&b).unwrap_or_else(|e| die(format!("{name}: {e:?}")));
        }
        let h = digest(&b);
        std::fs::write(blobs.join(hex(&h)), &b).unwrap_or_else(|e| die(e));
        println!("{}  {name}", hex(&h));
        map.insert(name, h);
    }
    if !map.contains_key(&entry) {
        die(format!("entry {entry} is not in {dir}"));
    }
    let m = SiteManifest { entry, files: map, mirrors };
    // The registry needs a rholang term; bytes are written as hex strings,
    // which the resolver accepts in place of bytes.
    let files_term: Vec<String> = m.files.iter().map(|(k, h)| format!("{k:?}: \"{}\"", hex(h))).collect();
    let mirrors_term: Vec<String> = m.mirrors.iter().map(|x| format!("{x:?}")).collect();
    let term = format!(
        "{{\"gaze\": 1, \"entry\": {:?}, \"files\": {{{}}}, \"mirrors\": [{}]}}\n",
        m.entry,
        files_term.join(", "),
        mirrors_term.join(", ")
    );
    std::fs::write(out.join("manifest.rho"), term).unwrap_or_else(|e| die(e));
    println!("wrote {}", out.display());
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        die("usage: f1r3c compile|inspect|integrity|site ... (see --help)");
    }
    let cmd = args.remove(0);
    let mut it = args.into_iter();
    match cmd.as_str() {
        "compile" => compile(it),
        "inspect" => inspect(&it.next().unwrap_or_else(|| die("inspect needs a file"))),
        "integrity" => println!("{}", load_knf(&it.next().unwrap_or_else(|| die("integrity needs a file"))).integrity()),
        "site" => site(it),
        "--version" => println!("f1r3c {}", env!("CARGO_PKG_VERSION")),
        _ => die("usage: f1r3c compile|inspect|integrity|site"),
    }
}
