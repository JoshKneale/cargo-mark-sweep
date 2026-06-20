use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::gc;

const QUIET_SECS: u64 = 60 * 60;
const OPP_BACKOFF_SECS: u64 = 20 * 60;
const THRESHOLD_BYTES: u64 = 10_000_000_000;
const RETRY_BACKOFF_SECS: u64 = 60 * 60;
const ACTIVE_POLL: Duration = Duration::from_secs(5);
const IDLE_POLL: Duration = Duration::from_secs(30);
const MAX_LEARNED_CMDS: usize = 8;

/// Coverage breadth of a mark config; higher subsumes lower. At cap, the
/// narrowest config is evicted so broad workspace marks (which reach the most
/// artifacts) survive rust-analyzer's per-package churn.
fn mark_breadth(cmd: &str) -> i32 {
    let mut s = 0;
    if cmd.contains("--workspace") {
        s += 2;
    }
    if cmd.contains("--all-targets") {
        s += 1;
    }
    if cmd.contains("--all-features") {
        s += 1;
    }
    if cmd.contains(" -p ") {
        s -= 2;
    }
    if cmd.contains("--test ") || cmd.contains("--bin ") || cmd.contains("--example ") {
        s -= 1;
    }
    s
}

pub struct DaemonArgs {
    pub once: bool,
    pub dry_run: bool,
    pub threshold_gb: Option<f64>,
    pub seeds: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    pub workspaces: BTreeMap<String, Workspace>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Workspace {
    #[serde(default)]
    pub cmds: Vec<String>,
    #[serde(default)]
    pub last_seen: u64,
    #[serde(default)]
    pub last_sweep: u64,
    #[serde(default)]
    pub last_freed: u64,
    #[serde(default)]
    pub last_attempt: u64,
    #[serde(default)]
    pub last_opportunistic: u64,
    #[serde(default)]
    pub dirty: bool,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME not set"))
}

pub fn state_path() -> PathBuf {
    home().join("Library/Application Support/cargo-mark-sweep/state.json")
}

impl State {
    pub fn load() -> State {
        fs::read(state_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> Result<(), String> {
        let path = state_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
        let json = serde_json::to_vec_pretty(self).expect("state serializes");
        fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs()
}

fn log(msg: &str) {
    eprintln!("[{}] {msg}", now());
}

/// Normalize an observed cargo command line into a markable configuration.
/// Returns None for invocations that don't build artifacts (fmt, metadata, our
/// own mark-sweep, rustc children, unrelated processes).
fn normalize(cmdline: &str) -> Option<String> {
    let tokens: Vec<&str> = cmdline.split_whitespace().collect();
    let argv0 = Path::new(tokens.first()?)
        .file_name()?
        .to_str()?
        .to_string();

    // `cargo test ...` or an external-subcommand child like `cargo-nextest nextest run ...`
    let mut rest: &[&str] = if argv0 == "cargo" || argv0.starts_with("cargo-") {
        &tokens[1..]
    } else {
        return None;
    };
    if let Some(first) = rest.first() {
        if first.starts_with('+') {
            rest = &rest[1..];
        }
    }
    let sub = *rest.first()?;
    if sub.starts_with('-') {
        return None;
    }
    let args = &rest[1..];

    let (mapped_sub, forced): (&str, &[&str]) = match sub {
        "build" | "check" | "clippy" | "doc" | "bench" => (sub, &[]),
        "test" => ("test", &["--no-run"]),
        "run" => ("build", &[]),
        // nextest flags don't translate to cargo test; mark the superset instead.
        "nextest" => return Some("test --no-run --workspace".into()),
        _ => return None,
    };

    let mut out: Vec<&str> = vec![mapped_sub];
    out.extend(forced);
    let mut it = args.iter();
    while let Some(&a) = it.next() {
        if a == "--" {
            break;
        }
        if a == "--message-format" || a == "--color" {
            it.next();
            continue;
        }
        if a.starts_with("--message-format=") || a.starts_with("--color=") {
            continue;
        }
        if a == "-q" || a == "--quiet" {
            continue;
        }
        if a == "--no-run" && forced.contains(&"--no-run") {
            continue;
        }
        out.push(a);
    }
    Some(out.join(" "))
}

/// pids and normalized commands of cargo build-ish processes currently running.
fn sample_cargo() -> Vec<(i32, String)> {
    let Ok(out) = Command::new("ps")
        .args(["-axww", "-o", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim_start();
        let Some((pid_str, cmdline)) = line.split_once(' ') else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<i32>() else {
            continue;
        };
        if pid == std::process::id() as i32 {
            continue;
        }
        if let Some(norm) = normalize(cmdline) {
            found.push((pid, norm));
        }
    }
    found
}

fn pid_cwd(pid: i32) -> Option<PathBuf> {
    let out = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n').map(PathBuf::from))
}

fn workspace_root(dir: &Path) -> Result<String, String> {
    let out = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .current_dir(dir)
        .output()
        .map_err(|e| format!("cargo locate-project: {e}"))?;
    if !out.status.success() {
        return Err(format!("not a cargo workspace: {}", dir.display()));
    }
    let manifest = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Path::new(&manifest)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| format!("bad manifest path: {manifest}"))
}

fn observe(state: &mut State, pid: i32, cmd: &str) -> bool {
    let Some(cwd) = pid_cwd(pid) else {
        return false;
    };
    let root = match workspace_root(&cwd) {
        Ok(r) => r,
        Err(e) => {
            log(&format!(
                "cannot resolve workspace for `cargo {cmd}` (pid {pid}): {e}"
            ));
            return false;
        }
    };
    let ws = state.workspaces.entry(root.clone()).or_default();
    ws.last_seen = now();
    let newly_dirty = !ws.dirty;
    ws.dirty = true;
    let new_cmd = !ws.cmds.iter().any(|c| c == cmd);
    if new_cmd {
        if ws.cmds.len() >= MAX_LEARNED_CMDS {
            let victim = ws
                .cmds
                .iter()
                .enumerate()
                .min_by_key(|(i, c)| (mark_breadth(c), *i))
                .map(|(i, _)| i)
                .expect("cmds non-empty at cap");
            ws.cmds.remove(victim);
        }
        ws.cmds.push(cmd.to_string());
        log(&format!("learned: `cargo {cmd}` in {root}"));
    } else if newly_dirty {
        log(&format!("activity in {root}"));
    }
    true
}

/// Deep passes (long idle) re-mark, sweep orphans, and wipe incremental/.
/// Opportunistic passes (any gap between builds) keep incremental/ — its wipe
/// taxes the next build ~22s, which mid-session is a real interruption.
fn sweep_pass(state: &mut State, dry_run: bool, threshold: u64, force: bool, deep: bool) {
    let keys: Vec<String> = state
        .workspaces
        .iter()
        .filter(|(_, w)| force || w.dirty)
        .map(|(k, _)| k.clone())
        .collect();

    for root in keys {
        let ws = state.workspaces.get_mut(&root).expect("key from same map");
        let (last_gate, backoff) = if deep {
            (ws.last_attempt, RETRY_BACKOFF_SECS)
        } else {
            (ws.last_opportunistic, OPP_BACKOFF_SECS)
        };
        if !force && now().saturating_sub(last_gate) < backoff {
            continue;
        }
        let target = match gc::target_directory(Path::new(&root)) {
            Ok(t) => t,
            Err(e) => {
                log(&format!("skip {root}: {e}"));
                ws.dirty = false;
                continue;
            }
        };
        let size = gc::entry_size(&target);
        if size < threshold {
            log(&format!(
                "{root}: target/ {} below threshold {}, skipping",
                gc::gb(size),
                gc::gb(threshold)
            ));
            ws.dirty = false;
            continue;
        }
        if gc::any_lock_held(&target) {
            log(&format!("{root}: cargo running, deferring"));
            continue;
        }
        if deep {
            ws.last_attempt = now();
        } else {
            ws.last_opportunistic = now();
        }
        let cmds = if ws.cmds.is_empty() {
            gc::DEFAULT_CMDS.iter().map(|s| s.to_string()).collect()
        } else {
            ws.cmds.clone()
        };
        log(&format!(
            "{} sweep: {root} (target/ {}, {} marked configs{})",
            if deep { "deep" } else { "opportunistic" },
            gc::gb(size),
            cmds.len(),
            if dry_run { ", dry run" } else { "" }
        ));
        match gc::run(&gc::Options {
            root: PathBuf::from(&root),
            cmds,
            dry_run,
            keep_incremental: !deep,
        }) {
            Ok(out) => {
                ws.dirty = false;
                ws.last_sweep = now();
                ws.last_freed = out.freed;
                for dead in &out.dead_cmds {
                    ws.cmds.retain(|c| c != dead);
                    log(&format!("{root}: pruned invalid config `cargo {dead}`"));
                }
                log(&format!(
                    "{root}: {} {}",
                    if dry_run { "reclaimable" } else { "freed" },
                    gc::gb(out.freed)
                ));
            }
            Err(e) => log(&format!(
                "{root}: sweep failed, will retry next window: {e}"
            )),
        }
    }
    if let Err(e) = state.save() {
        log(&format!("state save failed: {e}"));
    }
}

pub fn run(args: DaemonArgs) -> Result<(), String> {
    let mut state = State::load();
    for seed in &args.seeds {
        let root = workspace_root(seed)?;
        let ws = state.workspaces.entry(root.clone()).or_default();
        ws.last_seen = now();
        ws.dirty = true;
        log(&format!("seeded workspace {root}"));
    }
    if !args.seeds.is_empty() {
        state.save()?;
    }
    let threshold = args
        .threshold_gb
        .map(|g| (g * 1e9) as u64)
        .unwrap_or(THRESHOLD_BYTES);

    if args.once {
        sweep_pass(&mut state, args.dry_run, threshold, true, true);
        return Ok(());
    }

    log(&format!(
        "daemon started (opportunistic: first gap, ≥{}m apart; deep: {}m quiet; threshold {}{})",
        OPP_BACKOFF_SECS / 60,
        QUIET_SECS / 60,
        gc::gb(threshold),
        if args.dry_run { ", dry run" } else { "" }
    ));
    let mut last_activity = now();
    let mut seen_pids: HashSet<i32> = HashSet::new();
    loop {
        let procs = sample_cargo();
        if procs.is_empty() {
            seen_pids.clear();
            if state.workspaces.values().any(|w| w.dirty) {
                let deep = now().saturating_sub(last_activity) >= QUIET_SECS;
                sweep_pass(&mut state, args.dry_run, threshold, false, deep);
            }
            std::thread::sleep(IDLE_POLL);
        } else {
            last_activity = now();
            let mut changed = false;
            for (pid, cmd) in procs {
                if seen_pids.insert(pid) {
                    changed |= observe(&mut state, pid, &cmd);
                }
            }
            if changed {
                if let Err(e) = state.save() {
                    log(&format!("state save failed: {e}"));
                }
            }
            std::thread::sleep(ACTIVE_POLL);
        }
    }
}

#[cfg(target_os = "macos")]
pub fn print_state() {
    let state = State::load();
    if state.workspaces.is_empty() {
        println!("no workspaces discovered yet");
        return;
    }
    let ago = |ts: u64| -> String {
        if ts == 0 {
            return "never".into();
        }
        let d = now().saturating_sub(ts);
        match d {
            0..=119 => format!("{d}s ago"),
            120..=7199 => format!("{}m ago", d / 60),
            7200..=172799 => format!("{}h ago", d / 3600),
            _ => format!("{}d ago", d / 86400),
        }
    };
    for (root, ws) in &state.workspaces {
        println!("{root}");
        println!(
            "  seen {} | swept {} ({}) | opportunistic {} | {}",
            ago(ws.last_seen),
            ago(ws.last_sweep),
            gc::gb(ws.last_freed),
            ago(ws.last_opportunistic),
            if ws.dirty { "dirty" } else { "clean" }
        );
        for c in &ws.cmds {
            println!("  cargo {c}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{mark_breadth, normalize};

    #[test]
    fn breadth_keeps_broad_marks() {
        assert!(mark_breadth("test --no-run --workspace") > mark_breadth("check -p api --tests"));
        assert!(
            mark_breadth("check --workspace --all-targets --all-features")
                > mark_breadth("check --all-targets")
        );
        assert!(mark_breadth("check -p nonexistent-crate") < 0);
    }

    #[test]
    fn maps_build_like_commands() {
        assert_eq!(
            normalize("cargo build --workspace"),
            Some("build --workspace".into())
        );
        assert_eq!(
            normalize("/Users/x/.cargo/bin/cargo clippy --all-targets"),
            Some("clippy --all-targets".into())
        );
        assert_eq!(normalize("cargo +nightly check"), Some("check".into()));
        assert_eq!(
            normalize("cargo run --bin api"),
            Some("build --bin api".into())
        );
        assert_eq!(
            normalize("cargo build --release -q"),
            Some("build --release".into())
        );
    }

    #[test]
    fn test_becomes_no_run_and_drops_runner_args() {
        assert_eq!(
            normalize("cargo test -p api -- --nocapture"),
            Some("test --no-run -p api".into())
        );
        assert_eq!(
            normalize("cargo test --no-run --workspace"),
            Some("test --no-run --workspace".into())
        );
    }

    #[test]
    fn nextest_maps_to_superset_mark() {
        assert_eq!(
            normalize("cargo nextest run -p api --retries 2"),
            Some("test --no-run --workspace".into())
        );
        assert_eq!(
            normalize("/Users/x/.cargo/bin/cargo-nextest nextest run"),
            Some("test --no-run --workspace".into())
        );
    }

    #[test]
    fn strips_message_format() {
        assert_eq!(
            normalize("cargo check --workspace --message-format=json --all-targets"),
            Some("check --workspace --all-targets".into())
        );
        assert_eq!(
            normalize("cargo check --message-format json-diagnostic-rendered-ansi"),
            Some("check".into())
        );
    }

    #[test]
    fn ignores_non_building_processes() {
        assert_eq!(normalize("cargo fmt --all"), None);
        assert_eq!(normalize("cargo metadata --format-version 1"), None);
        assert_eq!(normalize("cargo mark-sweep --dry-run"), None);
        assert_eq!(normalize("/path/to/cargo-mark-sweep daemon"), None);
        assert_eq!(normalize("rustc --crate-name api"), None);
        assert_eq!(normalize("vim src/main.rs"), None);
    }
}
