# Classic adapter validation (2026-11-08)

Branch: `feat/classic-herdr-backend`, based on fork `ad23942`.
No commits, pushes, host binary installation, or live Herdr control performed.
No dependency changes; the existing pinned Ghostty submodule was initialized.
Both local debug variants were built and retained (not installed):
`target/variants/modern/herdr-core` and `target/variants/classic/herdr-core`.

## Real interoperability

Command: `bash experiments/classic/run.sh` (rebuilds the fork with
the classic-backend feature, not an upstream binary).

All 18 assertions passed against classic-only 0.9.5 over SSH with an ephemeral
pinned host key. Evidence from the final feature-enabled run:

- Original native pane: `w1:p1`, created deliberately by the fixture before add.
- Original runtime: `term_65fcc0af240f11`.
- Original shell PID and process group: `77`, `/bin/sh`, `/home/fixture`.
- Adapter discovered that exact pane/runtime and preserved workspace/tab IDs.
- Literal input plus explicit Enter was acknowledged, and `ADAPTER_VERIFIED pid=`
  appeared in the native recent-text read.
- Restarting the isolated local Core and discovering again preserved the runtime,
  original shell/process record and pane count; no focus mutation occurred.
- `hc` / `herdr-core` remained absent from the remote host before and after.
- Stock modern registration still failed with `hc_missing`; native classic still
  rejected a modern `hello`. Compatibility comes from the adapter, not a changed
  original-protocol test.
- Modern read/run/update against classic and stale runtime input were rejected.
- Classic panes were absent from the modern terminal inventory; inferred native
  agent status was absent from adapter discovery.

Run resources `hc-classic-1794179747-42937-{host,client}`, their network and all
fixture keys were cleaned. Docker listing confirmed no `hc-classic` containers
or networks remained. The built image/cache are retained for reproduction.

## Rust/schema/docs

Host toolchain: Rust 1.99.0, pinned Zig 0.16.0 downloaded only to scratch
`/var/tmp/hc-classic-tools` because host Zig 0.17.0 cannot build this Ghostty pin.
Docker compilation used Rust 1.95.0 and Zig 0.16.0.
Set `ZIG=/var/tmp/hc-classic-tools/zig-aarch64-macos-0.16.0/zig` for host commands.

Passed:

- `cargo fmt --all -- --check` and `git diff --check`.
- `cargo clippy --locked --all-targets -- -D warnings` in both default and
  classic-backend feature configurations.
- `HC_UPDATE_SCHEMA=1 cargo test --locked -p herdr-core --test m0` (10 tests).
- `HC_UPDATE_DOCS=1 cargo test --locked -p herdr-core --test cli_docs` (2 tests).
- Default-build classic tests: profile/param deserialization defaults, disabled
  adapter rejection without fallback, no classic capability advertised (2 tests).
- Feature build: 5 adapter unit tests and 2 classic integration/default tests.
  Tests cover correlation/native errors, bounds/partial responses, opaque IDs,
  vendor-status exclusion, unsupported attach/run/update, stale identity,
  acknowledgement loss with exactly one input attempt, rename/restart.
- Feature build library tests (140 passed, 1 ignored, plus its subprocess test)
  and `machines` (19), `remote` (20), `reconnect` (3), `resize_negotiation` (1),
  `viewer_activity` (1), `compat` (2), `m0` (10), `cli_docs` (2).
  `resize_compat` has two ignored tests; not claimed as executed.

Full default `cargo test --locked --no-fail-fast` ran all suites but was **not
fully green**: `m2::size_follows_the_last_active_typing_viewer` timed out waiting
for viewer geometry `30 100`. The exact test then passed in isolation and all
23 `m2` tests passed with one test thread. Earlier broad runs also showed
intermittent timing failures in `ghosts`/`m2`; no unrelated test behavior was
changed or failures hidden. All other suites passed in the final full run.

`just check` could not run: `just` is unavailable. Its component format/clippy/
tests commands were run directly as above. `cargo deny check` is also unavailable
(no cargo-deny). Windows clippy was attempted with the repository Zig stub but
blocked by the absent `x86_64-pc-windows-msvc` standard-library target. No Windows
runtime, graphics, mobile or real-agent smoke is claimed.

Detailed local logs remain in `/var/tmp/hc-classic-tools/` (`runtime-final.log`,
`tests-default-final.log`, `tests-feature-final.log`, `m2-serial.log`,
`clippy-default.log`, `clippy-feature.log`, `schema-final.log`, `docs-final.log`,
`windows.log`). They contain no fixture private keys. This scratch directory and
local build artifacts are retained; test sessions and Docker resources are not.

See [classic-backend.md](classic-backend.md) for unsupported capabilities and the
non-atomic identity-preflight limitation. This is a working control-plane slice,
not a complete classic surface/backend plugin.
