use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
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

pub struct RunError {
    pub message: String,
    /// Config that aborted the sweep when cargo's failure was neither structural
    /// nor a recognisable compile error. A compile error is transient (the tree
    /// is mid-edit) and must never count against a config; an unclassifiable
    /// failure may be a config that can no longer work, so the daemon counts it.
    pub suspect_cmd: Option<String>,
}

impl From<String> for RunError {
    fn from(message: String) -> Self {
        RunError {
            message,
            suspect_cmd: None,
        }
    }
}

impl From<&str> for RunError {
    fn from(message: &str) -> Self {
        RunError::from(message.to_string())
    }
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
    entry_size_seen(path, &mut HashSet::new())
}

/// Bytes under `path`, counting each inode at most once across the whole walk.
/// cargo hardlinks artifacts heavily (two thirds of a large `deps/` is links),
/// so summing every entry's length reports ~1.8x the space a delete can free.
pub fn entry_size_seen(path: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
            return 0;
        }
        return meta.len();
    }
    let mut total = 0;
    if let Ok(rd) = fs::read_dir(path) {
        for e in rd.flatten() {
            total += entry_size_seen(&e.path(), seen);
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

/// cargo reached compilation and a real package failed: the tree is broken, not
/// the config. Always transient — never counts against a config.
fn is_compile_error(stderr: &str) -> bool {
    stderr.contains("error[E") || stderr.contains("could not compile")
}

/// A non-zero cargo exit is "structural" — the config itself is unusable — only
/// when cargo never reached compilation. A real compile error (`error[E…]` /
/// `could not compile`) means the tree is broken: keep the config, abort.
fn is_structural(stderr: &str) -> bool {
    if is_compile_error(stderr) {
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
fn mark_one(root: &Path, cmd: &str, live_paths: &mut Vec<String>) -> Result<Mark, RunError> {
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
        return Err(RunError {
            message: format!(
                "`cargo {cmd}` failed ({}); aborting before any sweep.\n{stderr}",
                out.status
            ),
            suspect_cmd: (!is_compile_error(&stderr)).then(|| cmd.to_string()),
        });
    }
    Ok(Mark::Ok)
}

/// Tracks which inodes a sweep removes links to, so `freed` can mean bytes the
/// filesystem actually gets back rather than bytes the deleted paths occupied.
///
/// Deleting one link to a multiply-linked inode frees nothing — the data lives
/// until the last link goes, and cargo hardlinks artifacts to places outside the
/// swept dirs (uplifted binaries in `target/<profile>/`). Occupancy and reclaim
/// therefore differ, and only reclaim is worth reporting as freed.
#[derive(Default)]
struct LinkLedger {
    /// (dev, ino) -> (size, links on disk, links this sweep removes)
    inodes: std::collections::HashMap<(u64, u64), (u64, u64, u64)>,
}

impl LinkLedger {
    /// Record every regular file under `path` as about to be unlinked.
    fn record(&mut self, path: &Path) {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return;
        };
        if meta.is_dir() {
            if let Ok(rd) = fs::read_dir(path) {
                for e in rd.flatten() {
                    self.record(&e.path());
                }
            }
            return;
        }
        let slot =
            self.inodes
                .entry((meta.dev(), meta.ino()))
                .or_insert((meta.len(), meta.nlink(), 0));
        slot.2 += 1;
    }

    /// Bytes released: an inode counts only once every link to it is gone.
    fn reclaimed(&self) -> u64 {
        self.inodes
            .values()
            .filter(|(_, nlink, removed)| removed >= nlink)
            .map(|(size, _, _)| size)
            .sum()
    }
}

/// Free bytes on the filesystem holding `path`, for corroborating `reclaimed()`
/// against what the filesystem actually reports.
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(s.f_bavail as u64 * s.f_bsize as u64)
}

struct SweepStats {
    live: u64,
    orphan: u64,
    removed: usize,
    errors: usize,
}

fn sweep_dir(
    dir: &Path,
    live: &HashSet<String>,
    dry_run: bool,
    seen: &mut HashSet<(u64, u64)>,
    ledger: &mut LinkLedger,
) -> SweepStats {
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
        let size = entry_size_seen(&path, seen);
        match first_hash(&name) {
            Some(h) if !live.contains(h) => {
                st.orphan += size;
                ledger.record(&path);
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

pub fn run(opts: &Options) -> Result<Outcome, RunError> {
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
    let mut errors = 0;
    let mut seen = HashSet::new();
    let mut ledger = LinkLedger::default();
    let free_before = free_bytes(&target);
    for profile in &profiles {
        let pdir = target.join(profile);
        for sub in ["deps", "build"] {
            let st = sweep_dir(&pdir.join(sub), &live, opts.dry_run, &mut seen, &mut ledger);
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
            let size = entry_size_seen(&incr, &mut seen);
            if opts.keep_incremental {
                eprintln!("  {profile}/incremental: {} (kept)", gb(size));
            } else {
                ledger.record(&incr);
                if !opts.dry_run {
                    if let Err(e) = fs::remove_dir_all(&incr) {
                        eprintln!("    failed to remove {}: {e}", incr.display());
                        errors += 1;
                    }
                }
                eprintln!("  {profile}/incremental: {} wiped", gb(size));
            }
        }
    }
    drop(locks);
    let freed = ledger.reclaimed();
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
        // Corroborate against the filesystem while the tree is still untouched —
        // the shakedown below rebuilds and would muddy the delta.
        if let (Some(before), Some(after)) = (free_before, free_bytes(&target)) {
            eprintln!("  filesystem freed: {}", gb(after.saturating_sub(before)));
        }

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
                )
                .into());
            }
            Err(e) => return Err(format!("failed to run shakedown: {e}").into()),
        }
    }

    if errors > 0 {
        return Err(format!("{errors} removal errors").into());
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
    fn only_compile_errors_are_transient() {
        assert!(is_compile_error(
            "error[E0433]: failed to resolve: could not find `X`"
        ));
        assert!(is_compile_error(
            "error: could not compile `engine` due to 1 previous error"
        ));
        // A broken tree must never count against a config, or a long mid-edit
        // window would retire good configs.
        assert!(!is_compile_error(
            "error: the `-Z` flag is only accepted on the nightly channel of Cargo"
        ));
        assert!(!is_compile_error("error: no such subcommand: `frobnicate`"));
    }

    #[test]
    fn reclaim_credits_only_fully_unlinked_inodes() {
        let dir = std::env::temp_dir().join(format!("cms-reclaim-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let doomed = dir.join("doomed");
        let elsewhere = dir.join("elsewhere");
        fs::create_dir_all(&doomed).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();

        // Both links inside the swept set: the data really goes.
        fs::write(doomed.join("a.rlib"), vec![0u8; 4096]).unwrap();
        fs::hard_link(doomed.join("a.rlib"), doomed.join("b.rlib")).unwrap();
        // Sole link: goes.
        fs::write(doomed.join("c.rlib"), vec![0u8; 1024]).unwrap();
        // Uplifted-style link surviving outside the swept set: frees nothing.
        fs::write(doomed.join("d.rlib"), vec![0u8; 8192]).unwrap();
        fs::hard_link(doomed.join("d.rlib"), elsewhere.join("d")).unwrap();

        let mut ledger = LinkLedger::default();
        ledger.record(&doomed);
        assert_eq!(ledger.reclaimed(), 4096 + 1024);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hardlinks_counted_once() {
        let dir = std::env::temp_dir().join(format!("cms-size-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.rlib"), vec![0u8; 4096]).unwrap();
        fs::hard_link(dir.join("a.rlib"), dir.join("b.rlib")).unwrap();
        fs::write(dir.join("c.rlib"), vec![0u8; 1024]).unwrap();

        assert_eq!(entry_size(&dir), 4096 + 1024);

        let mut seen = HashSet::new();
        assert_eq!(entry_size_seen(&dir.join("a.rlib"), &mut seen), 4096);
        assert_eq!(entry_size_seen(&dir.join("b.rlib"), &mut seen), 0);

        fs::remove_dir_all(&dir).unwrap();
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
