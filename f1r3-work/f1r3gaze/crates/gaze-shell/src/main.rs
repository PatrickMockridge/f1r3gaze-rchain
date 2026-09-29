//! `f1r3gaze` — the browser.
//!
//! ```text
//! f1r3gaze [URL]                     open a window
//! f1r3gaze --headless URL [--allow] [--click SELECTOR]... [--timeout SECS]
//!          [--log FILE.gzlog]        run a page without a window and print
//!                                    its committed document and console
//! f1r3gaze --profile DIR ...         use DIR as the profile
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
