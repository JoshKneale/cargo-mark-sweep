---
name: Bug report
about: Something deleted, missed, or behaved unexpectedly
title: ""
labels: bug
assignees: ""
---

## What happened
<!-- What went wrong. If files were deleted that shouldn't have been, say so up front. -->

## Did you run `--dry-run` first?
<!-- This tool deletes files. `cargo mark-sweep --dry-run` prints exactly what it would
     remove. Pasting that output is the single most useful thing for diagnosing. -->

## Command used
<!-- The exact invocation, including any --cmd / --keep-incremental flags and workspace path. -->

```
cargo mark-sweep ...
```

## Expected vs actual

- Expected:
- Actual:

## Environment

- OS / arch: <!-- e.g. macOS 14 arm64, Ubuntu 24.04 x86_64. Note: daemon mode is macOS-only. -->
- `cargo --version`:
- `rustc --version`:
- cargo-mark-sweep version: <!-- `cargo mark-sweep --version` or the crate version -->

## Anything else
<!-- Logs, shakedown output, relevant target/ layout. -->
