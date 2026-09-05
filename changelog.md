# Focused passes

## 2026-09-04 — Refactor: agent configuration loading

- Shared `.rdp` loading and ordered `--prop` application between `connect` and `daemon-start`; removed the now-single-use override wrapper. Named flags still win, malformed entries still warn, and file-read failures remain fatal.
- Validated on macOS with unpinned nightly: release build; 12 existing library tests and 1 daemon IPC test; 8 before/after release-CLI scenarios with identical decoded Connect properties and diagnostics (local IPC fixture, no live RDP server), including missing-file/invalid-property negative controls; Clippy completed.
- Existing issues: rustfmt check reports untouched import layout in agent `cli.rs` and `gw_forward/socks5.rs`; connector `connection.rs:1646` has an unfulfilled lint expectation. Windows/Linux runtime paths were not exercised.
- Next pass: bug hunt in a different subsystem; no root pass history existed before this entry.
