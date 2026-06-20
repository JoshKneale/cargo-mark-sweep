# cargo-mark-sweep

**A mark-and-sweep garbage collector for cargo's `target/` directory.**

Reclaims the 90%+ of `target/` that cargo will never use again, without touching
anything your builds still need — no cold rebuild, caches stay warm.

## Safety

This tool **deletes files** under `target/`. It is built around one invariant: it
only deletes artifacts that the cargo commands you configure do **not** resolve to,
and it proves the live set is intact before and after:

- A failed mark command (your code doesn't compile) **aborts the whole run before
  any deletion**.
- After sweeping it runs a **shakedown** — re-runs your first command and fails
  loudly if anything is actually broken.
- It never touches `.fingerprint/` (deleting those invalidates live crates).

That said: it has been validated in a single environment (see [Status](#status)).
**Run `cargo mark-sweep --dry-run` first** to see exactly what it would delete. The
one-shot command deletes for real by default. If something looks wrong, please
[open an issue](https://github.com/joshkneale/cargo-mark-sweep/issues).

## Why

Cargo's `target/` is an append-only cache: artifact filenames embed a hash of
(features, dependency graph, profile, compile mode), and any change writes a new
file next to the old one. Nothing ever deletes the old ones. On an actively
developed workspace with integration tests this compounds fast — every edit→test
cycle gives every test binary a new hash (measured: 50–100 GB/day; one workday left
92% of `deps/` orphaned and 98% of `incremental/` stale).

Existing tools either nuke everything (`cargo clean`, kondo) or sweep by mtime
(cargo-sweep), which deletes live artifacts and misses fresh orphans because cargo
doesn't touch mtimes on reuse.

## How

**Mark**: run your daily build configurations as no-op warm builds with
`--message-format=json`. Cargo emits a `compiler-artifact` message for every unit —
including fresh ones — naming the exact files that configuration resolves to.
Stable public interface; no cargo internals parsed. Seconds per command on a warm
cache.

**Sweep**: delete entries in `target/<profile>/{deps,build}` whose filename hash
isn't in the live set, and wipe `incremental/` wholesale (measured ~98% stale unit
dirs; regenerates per edit at a one-time cost of seconds). `.fingerprint/` is never
touched: some unit types have fingerprint hashes that appear in no artifact
filename, and deleting them invalidates live crates.

**Shakedown**: re-run the first command to absorb the one-time post-sweep
recompile, and fail loudly if anything is actually broken.

## Install

```bash
cargo install cargo-mark-sweep
```

## Usage

```bash
cargo mark-sweep --dry-run          # recommended first: report what would be deleted
cargo mark-sweep                    # mark, sweep, shakedown in the cwd workspace (deletes)
cargo mark-sweep --keep-incremental # keep incremental/ (skip its wipe)
cargo mark-sweep --cmd "build --workspace" --cmd "test --no-run -p api"
                                    # override the marked configurations
```

Anything you don't mark gets swept and will rebuild next time you need it — the cost
is a rebuild, never breakage; a failed mark command aborts before any deletion.

Default marked configurations:

- `cargo build --workspace`
- `cargo test --no-run --workspace`
- `cargo clippy --all-targets --all-features --workspace`

## Platform support

| Mode                              | macOS | Linux | Windows |
| --------------------------------- | :---: | :---: | :-----: |
| One-shot `cargo mark-sweep`       |   ✓   |   ✓   |   —     |
| Background daemon (`enable`)      |   ✓   |   —   |   —     |

The one-shot command is portable across Unix. The resident daemon is **macOS only**
(it uses launchd); on other platforms `enable`/`disable`/`status` report that and
exit.

## Background daemon (macOS only, experimental)

```bash
cargo mark-sweep enable             # install + start launchd agent (add --dry-run to report only)
cargo mark-sweep status             # daemon state, discovered workspaces, learned configs
cargo mark-sweep disable
```

The daemon needs no configuration. It samples running processes for cargo
invocations, learning both the workspaces you build in and the configurations
you actually use (normalized: `test` → `test --no-run`, `nextest` → the test
superset, runner args stripped). Two sweep modes, both gated on target/ > 10 GB
and the build lock being free, both holding cargo's own `.cargo-lock` during the
sweep so a build started mid-sweep blocks normally instead of racing:

- **Opportunistic** — fires in the first gap between builds, at most every
  20 minutes per workspace. Keeps `incremental/` (its wipe taxes the next build
  ~22 s — a real interruption mid-session). Marks are warm seconds because they
  run right after the builds that warmed them. This is what keeps target/ flat
  while agents iterate continuously.
- **Deep** — fires after 60 minutes of no cargo activity anywhere (lunch,
  overnight). Also wipes `incremental/`.

State: `~/Library/Application Support/cargo-mark-sweep/state.json`.
Log: `~/Library/Logs/cargo-mark-sweep.log`.
Testing hook: `cargo mark-sweep daemon --once --dry-run [--threshold-gb N] <workspace>`.

## Status

Early release. Validated in a **single environment** (one developer's machine, macOS)
on one large private workspace (11 crates, 686 dependencies, 86 integration-test
binaries) under continuous build load:

- ~9 days running live with real deletions
- **187 GB reclaimed across 45 sweeps**
- **zero failed shakedowns, zero live-artifact deletions, zero `.fingerprint` touches**

This is a single-environment track record, not broad field testing — behavior on
other toolchain versions, custom `CARGO_TARGET_DIR` layouts, vendored/sparse
registries, and non-macOS platforms is unproven. Bug reports and dry-run findings
from other setups are very welcome.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
