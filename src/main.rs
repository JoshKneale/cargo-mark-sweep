//! cargo-mark-sweep: reachability-based GC for cargo target directories.
//!
//! One-shot: `cargo mark-sweep [opts] [workspace]` marks artifacts reachable
//! from the configured cargo commands, then sweeps unreachable hash variants
//! from target/<profile>/{deps,build} and wipes incremental/.
//!
//! Daemon: `cargo mark-sweep enable` installs a launchd agent that watches for
//! cargo activity (ps sampling), learns workspaces and build configurations
//! from observed invocations, and sweeps during quiet moments once target/
//! exceeds a size threshold. `disable` and `status` complete the lifecycle.

mod daemon;
mod gc;
mod launchd;

use std::env;
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!(
        "Usage:\n\
         \x20 cargo mark-sweep [--dry-run] [--keep-incremental] [--cmd \"<cargo args>\"]... [workspace]\n\
         \x20 cargo mark-sweep enable [--dry-run]    install + start the background daemon (launchd)\n\
         \x20 cargo mark-sweep disable               stop + remove the daemon\n\
         \x20 cargo mark-sweep status                daemon state, workspaces, learned configs\n\
         \x20 cargo mark-sweep daemon [--once] [--dry-run] [--threshold-gb N] [workspace]...\n\
         \n\
         Default one-shot mark commands:\n\
         {}",
        gc::DEFAULT_CMDS
            .iter()
            .map(|c| format!("  cargo {c}\n"))
            .collect::<String>()
    );
    std::process::exit(2);
}

fn one_shot(argv: &[String]) -> Result<(), String> {
    let mut opts = gc::Options {
        root: PathBuf::from("."),
        cmds: Vec::new(),
        dry_run: false,
        keep_incremental: false,
    };
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dry-run" | "-n" => opts.dry_run = true,
            "--keep-incremental" => opts.keep_incremental = true,
            "--cmd" => match it.next() {
                Some(c) => opts.cmds.push(c.clone()),
                None => usage(),
            },
            "--help" | "-h" => usage(),
            p if !p.starts_with('-') => opts.root = PathBuf::from(p),
            _ => usage(),
        }
    }
    if opts.cmds.is_empty() {
        opts.cmds = gc::DEFAULT_CMDS.iter().map(|s| s.to_string()).collect();
    }
    gc::run(&opts).map(|_| ()).map_err(|e| e.message)
}

fn daemon_args(argv: &[String]) -> daemon::DaemonArgs {
    let mut args = daemon::DaemonArgs {
        once: false,
        dry_run: false,
        threshold_gb: None,
        seeds: Vec::new(),
    };
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--once" => args.once = true,
            "--dry-run" | "-n" => args.dry_run = true,
            "--threshold-gb" => match it.next().and_then(|v| v.parse().ok()) {
                Some(g) => args.threshold_gb = Some(g),
                None => usage(),
            },
            p if !p.starts_with('-') => args.seeds.push(PathBuf::from(p)),
            _ => usage(),
        }
    }
    args
}

fn main() {
    let mut argv: Vec<String> = env::args().skip(1).collect();
    // Invoked as `cargo mark-sweep` -> first arg is "mark-sweep"; skip it.
    if argv.first().map(String::as_str) == Some("mark-sweep") {
        argv.remove(0);
    }

    let result = match argv.first().map(String::as_str) {
        Some("enable") => launchd::enable(argv[1..].iter().any(|a| a == "--dry-run")),
        Some("disable") => launchd::disable(),
        Some("status") => launchd::status(),
        Some("daemon") => daemon::run(daemon_args(&argv[1..])),
        _ => one_shot(&argv),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
