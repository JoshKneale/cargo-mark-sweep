# Security Policy

## Supported versions

Only the latest published release on [crates.io](https://crates.io/crates/cargo-mark-sweep)
receives fixes. Pin to a released version rather than `main`.

## Reporting a vulnerability

Please report security issues **privately**, not as a public issue:

- Preferred: open a [private security advisory](https://github.com/joshkneale/cargo-mark-sweep/security/advisories/new).
- Alternatively, email joshkneale89@gmail.com.

Since this tool deletes files under `target/`, reports about data loss outside the
documented invariants (touching `.fingerprint/`, deleting reachable artifacts, or a
shakedown that fails to catch a broken live set) are treated as security-relevant.

Expect an initial response within a few days. There is no bounty program.
