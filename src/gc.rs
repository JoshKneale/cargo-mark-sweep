use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const DEFAULT_CMDS: &[&str] = &[
    "build --workspace",
    "test --no-run --workspace",
    "clippy --all-targets --all-features --workspace",
];

pub struct Options {
    pub root: PathBuf,
    pub cmds: Vec<String>,
    pub dry_run: bool,
    pub keep_incremental: bool,
}

pub struct Outcome {
    pub freed: u64,
    pub dead_cmds: Vec<String>,
}

pub fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / 1e9)
}

/// The 16-hex-char hash cargo appends to artifact names, delimited by a leading
/// '-' and a trailing '.', '/', or end of string.
fn hashes_in<'a>(s: &'a str, terminators: &[u8]) -> Vec<&'a str> {
    let b = s.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while let Some(dash) = s[i..].find('-').map(|d| i + d) {
        let start = dash + 1;
        let end = start + 16;
        if end <= b.len()
            && b[start..end].iter().all(u8::is_ascii_hexdigit)
            && b[start..end].iter().any(u8::is_ascii_digit)
            && (end == b.len() || terminators.contains(&b[end]))
        {
            found.push(&s[start..end]);
            i = end;
        } else {
            i = start;
        }
    }
    found
}

fn first_hash(name: &str) -> Option<&str> {
    hashes_in(name, b".").into_iter().next()
}

pub fn target_directory(root: &Path) -> Result<PathBuf, String> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("failed to run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let meta: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("bad cargo metadata json: {e}"))?;
    meta["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| "cargo metadata missing target_directory".into())
}

pub fn entry_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0;
    if let Ok(rd) = fs::read_dir(path) {
        for e in rd.flatten() {
            total += entry_size(&e.path());
        }
    }
    total
}

/// Take cargo's own build lock for a profile dir, non-blocking. While held, any
/// cargo invocation against this profile blocks with cargo's normal "Blocking
/// waiting for file lock" message instead of racing the sweep.
fn try_lock_profile(target: &Path, profile: &str) -> Result<File, String> {
    let path = target.join(profile).join(".cargo-lock");
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(f)
    } else {
        Err(format!("build lock held on {}", path.display()))
    }
}

/// True if any profile's build lock under target/ is currently held by cargo.
pub fn any_lock_held(target: &Path) -> bool {
    let Ok(rd) = fs::read_dir(target) else {
        return false;
    };
    for e in rd.flatten() {
        let lock = e.path().join(".cargo-lock");
        if lock.is_file() {
            match OpenOptions::new().read(true).write(true).open(&lock) {
                Ok(f) => {
                    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                    if rc != 0 {
                        return true;
                    }
                }
                Err(_) => return true,
            }
        }
    }
    false
}

/// Result of marking with one config.
enum Mark {
    Ok,
    /// Config can never match this workspace (stale `-p`, removed flag, missing
    /// manifest). It enumerates nothing and says nothing about tree health, so
    /// the sweep skips it and the daemon prunes it. Distinct from a compile
    /// failure, which means a real package is broken and the sweep must abort.
    Dead(String),
}

/// A non-zero cargo exit is "structural" — the config itself is unusable — only
/// when cargo never reached compilation. A real compile error (`error[E…]` /
/// `could not compile`) means the tree is broken: keep the config, abort.
fn is_structural(stderr: &str) -> bool {
    if stderr.contains("error[E") || stderr.contains("could not compile") {
        return false;
    }
    const PATTERNS: &[&str] = &[
        "did not match any packages",
        "no matching package named",
        "manifest path",
        "could not find `Cargo.toml`",
        "unexpected argument",
        "Unrecognized option",
        "no such subcommand",
        "is unstable",
        "only accepted on the nightly channel",
    ];
    PATTERNS.iter().any(|p| stderr.contains(p))
}

/// Run one cargo command with JSON messages; collect live paths. Aborts on
/// command failure with stderr shown (a failed mark must never lead to a sweep).
fn mark_one(root: &Path, cmd: &str, live_paths: &mut Vec<String>) -> Result<Mark, String> {
    eprintln!("  marking: cargo {cmd}");
    let mut child = Command::new("cargo")
        .args(cmd.split_whitespace())
        .arg("--message-format=json-render-diagnostics")
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn cargo {cmd}: {e}"))?;

    let stdout = child.stdout.take().expect("piped stdout");
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|e| format!("read error: {e}"))?;
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match msg["reason"].as_str() {
            Some("compiler-artifact") => {
                if let Some(files) = msg["filenames"].as_array() {
                    live_paths.extend(files.iter().filter_map(|f| f.as_str().map(String::from)));
                }
                if let Some(exe) = msg["executable"].as_str() {
                    live_paths.push(exe.to_string());
                }
            }
            Some("build-script-executed") => {
                if let Some(out_dir) = msg["out_dir"].as_str() {
                    live_paths.push(out_dir.to_string());
                }
            }
            _ => {}
        }
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("cargo {cmd} did not exit cleanly: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if is_structural(&stderr) {
            let detail = stderr
                .lines()
                .find(|l| l.contains("error"))
                .unwrap_or("")
                .trim();
            return Ok(Mark::Dead(format!("`cargo {cmd}`: {detail}")));
        }
        return Err(format!(
            "`cargo {cmd}` failed ({}); aborting before any sweep.\n{stderr}",
            out.status
        ));
    }
    Ok(Mark::Ok)
}

struct SweepStats {
    live: u64,
    orphan: u64,
    removed: usize,
    errors: usize,
}

fn sweep_dir(dir: &Path, live: &HashSet<String>, dry_run: bool) -> SweepStats {
    let mut st = SweepStats {
        live: 0,
        orphan: 0,
        removed: 0,
        errors: 0,
    };
    let Ok(rd) = fs::read_dir(dir) else {
        return st;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let size = entry_size(&path);
        match first_hash(&name) {
            Some(h) if !live.contains(h) => {
                st.orphan += size;
                if dry_run {
                    st.removed += 1;
                    continue;
                }
                let res = if path.is_dir() {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                };
                match res {
                    Ok(()) => st.removed += 1,
                    Err(e) => {
                        eprintln!("    failed to remove {}: {e}", path.display());
                        st.errors += 1;
                    }
                }
            }
            _ => st.live += size, // live hash, or unhashed entry: keep
        }
    }
    st
}

pub fn run(opts: &Options) -> Result<Outcome, String> {
    let target = target_directory(&opts.root)?;

    eprintln!("== Mark ==");
    let mut live_paths = Vec::new();
    let mut dead_cmds = Vec::new();
    let mut usable = 0;
    for cmd in &opts.cmds {
        match mark_one(&opts.root, cmd, &mut live_paths)? {
            Mark::Ok => usable += 1,
            Mark::Dead(why) => {
                eprintln!("  skipping invalid config: {why}");
                dead_cmds.push(cmd.clone());
            }
        }
    }
    if usable == 0 {
        return Err("no usable mark configs — refusing to sweep".into());
    }

    let mut live = HashSet::new();
    let mut profiles = HashSet::new();
    let target_str = target.to_string_lossy().into_owned();
    for p in &live_paths {
        for h in hashes_in(p, b"./") {
            live.insert(h.to_string());
        }
        if let Some(rest) = p.strip_prefix(&target_str) {
            if let Some(profile) = rest.split('/').find(|c| !c.is_empty()) {
                profiles.insert(profile.to_string());
            }
        }
    }
    if live.is_empty() {
        return Err("mark produced no live artifacts — refusing to sweep".into());
    }
    eprintln!(
        "  {} live file refs, {} distinct hashes, profiles touched: {:?}",
        live_paths.len(),
        live.len(),
        profiles
    );

    let mut locks = Vec::new();
    for profile in &profiles {
        locks.push(try_lock_profile(&target, profile)?);
    }

    let mode = if opts.dry_run { " (dry run)" } else { "" };
    eprintln!("\n== Sweep{mode} ==");
    let mut freed: u64 = 0;
    let mut errors = 0;
    for profile in &profiles {
        let pdir = target.join(profile);
        for sub in ["deps", "build"] {
            let st = sweep_dir(&pdir.join(sub), &live, opts.dry_run);
            freed += st.orphan;
            errors += st.errors;
            eprintln!(
                "  {profile}/{sub}: live {}, orphan {} ({} entries{})",
                gb(st.live),
                gb(st.orphan),
                st.removed,
                if st.errors > 0 {
                    format!(", {} ERRORS", st.errors)
                } else {
                    String::new()
                }
            );
        }
        let incr = pdir.join("incremental");
        if incr.is_dir() {
            let size = entry_size(&incr);
            if opts.keep_incremental {
                eprintln!("  {profile}/incremental: {} (kept)", gb(size));
            } else {
                if !opts.dry_run {
                    if let Err(e) = fs::remove_dir_all(&incr) {
                        eprintln!("    failed to remove {}: {e}", incr.display());
                        errors += 1;
                    }
                }
                freed += size;
                eprintln!("  {profile}/incremental: {} wiped", gb(size));
            }
        }
    }
    drop(locks);
    eprintln!(
        "\n  {} {}",
        if opts.dry_run {
            "reclaimable:"
        } else {
            "freed:"
        },
        gb(freed)
    );

    if !opts.dry_run {
        // Shakedown: prove the live set still builds; first build after a sweep
        // may recompile a handful of workspace units.
        eprintln!("\n== Shakedown ==");
        let first = &opts.cmds[0];
        let status = Command::new("cargo")
            .args(first.split_whitespace())
            .current_dir(&opts.root)
            .status();
        match status {
            Ok(s) if s.success() => eprintln!("  cargo {first}: OK"),
            Ok(s) => {
                return Err(format!(
                    "shakedown `cargo {first}` failed ({s}) — investigate before trusting this sweep"
                ));
            }
            Err(e) => return Err(format!("failed to run shakedown: {e}")),
        }
    }

    if errors > 0 {
        return Err(format!("{errors} removal errors"));
    }
    Ok(Outcome { freed, dead_cmds })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_vs_compile_failures() {
        assert!(is_structural(
            "error: package ID specification `nonexistent-crate` did not match any packages"
        ));
        assert!(is_structural("error: Unrecognized option: 'frobnicate'"));
        assert!(!is_structural(
            "error[E0433]: failed to resolve: could not find `X`"
        ));
        assert!(!is_structural(
            "error: could not compile `engine` due to 1 previous error"
        ));
        assert!(!is_structural(
            "did not match any packages\nerror[E0001]: x"
        ));
        assert!(is_structural(
            "error: the `-Z` flag is only accepted on the nightly channel of Cargo, but this is the `stable` channel"
        ));
        assert!(!is_structural(
            "error: the `-Z` flag is only accepted on the nightly channel\nerror[E0433]: x"
        ));
    }

    #[test]
    fn finds_artifact_hashes() {
        assert_eq!(
            first_hash("libserde-9f8e7d6c5b4a3210.rlib"),
            Some("9f8e7d6c5b4a3210")
        );
        assert_eq!(first_hash("CACHEDIR.TAG"), None);
        assert_eq!(
            hashes_in("deps/api-0123456789abcdef", b"./"),
            vec!["0123456789abcdef"]
        );
    }
}
