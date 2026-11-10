# Optional classic control backend

This fork retains modern Core. The opt-in `classic-backend` Cargo feature adds a
fresh, narrow JSON control client for classic Herdr **0.9.5 / protocol 22**. It does
not embed classic source, install Core remotely, or implement a multiplexer.

## Two builds

```sh
cargo build --locked -p herdr-core --target-dir target/modern
cargo build --locked -p herdr-core --features classic-backend --target-dir target/classic
```

Use `target/modern/debug/herdr-core` for normal modern server deployment and
`target/classic/debug/herdr-core` locally when classic compatibility is needed.
Neither command installs or restarts anything. The feature-enabled binary still
supports modern peers. It is **not** installed on the classic host.

Both builds expose the same schema/profile format. Only the enabled build compiles
the classic transport and advertises `classic_control` in `hello.features`.
The default build returns `backend_disabled` for classic requests and preserves
saved classic profiles without dialing them. Use separate local `HC_SESSION`s
if running both builds; starting a different CLI binary does not replace an
already-running Core in that session.

## Usage

With the feature-enabled binary (`hc` below), an already-running remote classic
session `work`, and normally verified SSH access:

```sh
hc machine add vps --name native --backend classic --session work
hc classic list native
hc classic read native w1:p1 term_RETURNED_BY_LIST --lines 100
hc classic send native w1:p1 term_RETURNED_BY_LIST --text 'printf hello' --key enter
```

Substitute the exact pane and terminal runtime IDs returned by discovery.
`--text` is literal input, not an SSH shell fragment; Enter is never implicit.
`--key` repeats and uses native classic key names. Send returns `accepted: true`
only after the native acknowledgement; this does not mean command completion or
success. Read returns `read.text`, native associations, revision and truncation.

`machine login` probes classic again after login; rename/remove only affect the
local profile. `machine update` and `--install` reject classic. There is no modern
fallback, remote bootstrap, or automatic upgrade.

## Architecture and identity

- `src/classic.rs`: feature-gated codec/SSH exchanges, bounds, typed projections,
  observed runtime-ID correlation, and disabled-build stubs.
- `machines.rs`: explicit backend, defaulting to modern for old profiles.
- `api.rs`, `api/types.rs`, `cli/machine.rs`: one API table and CLI params;
  `classic.list`, `classic.read`, `classic.send` are separate from Core terminals.
- `remote.rs`: classic is on-demand, excluded from modern supervision/inventory;
  modern routed operations (including attach) fail with a capability error.

A saved classic profile identifies an SSH target/command plus a named session.
SSH authenticates the host using normal host-key verification. No permanent Core
identity or classic boot ID is invented: the legacy `id` field remains an empty
string, while `host` is the configured SSH target. Pane, runtime, workspace and tab
IDs remain opaque native strings, never numeric local holders or recursively
rewritten addresses. Backend metadata records associations, not layout ownership.
Classic inferred agent status is deliberately discarded; OSC 7501 remains Core's
only status source.

Every operation opens bounded SSH exchanges to
`herdr --session NAME remote-api-bridge`. Probes check version and protocol;
there is no persistent event stream. `machine ls` reports `on_demand`, not a claim
that a connection is alive (`backend_disabled` in the default build).

## Safety and limits

Each exchange has a 20-second deadline and a 1 MiB request/response limit. Reads
request 1–1000 recent lines. Input accepts at most 64 KiB text and 64 key names
of at most 64 bytes. Requests and replies are correlated; malformed/partial or
oversized replies are rejected. Input is never retried after a lost reply;
`classic_outcome_unknown` means inspect before deciding whether to retry.

Read/input preflight compares the supplied pane/runtime pair with fresh discovery.
This detects an **observed** mismatch, not atomic boot fencing: JSON input has no
expected-runtime field. Concurrent native pane replacement between preflight and
input remains a race. Avoid automating input across such replacement. A stable
client codec with boot fencing is future work, not emulated here.

Discovery/read never call focus, resize, scroll or `layout.apply`. Input does not
invoke focus. Public classic focus can affect other clients and is not exposed.

## Support matrix

| Surface | First slice |
| --- | --- |
| Existing native panes and runtime associations | Supported/tested |
| Recent text read; explicit text/keys | Supported/tested |
| Reconnect without killing/duplicating native shell | Supported/tested |
| Modern-to-modern runtime | Preserved; regression tested |
| Classic pane/workspace creation | Unsupported; deliberate native setup only in fixture |
| Modern run/wait/recovery/files | Unsupported on classic |
| Attach/raw bytes, cell surfaces, resize, graphics | Unsupported; no fake raw translation |
| Native status/brand inference | Not exposed |
| Harness/chat/artifact protocols | Not implemented |
| Mobile builds/transports | Not built or claimed; SSH subprocess transport remains desktop-only |

## Reproduce interoperability

`experiments/classic/run.sh` builds this modified checkout with the feature and
runs a pinned classic-only SSH host. It requires the sibling research fixture
images `hydra-compat-classic:0.9.5` and `hydra-compat-modern:c6b316f` (the latter is
only a client runtime base; its Core binary is replaced by the fork build).
Current Dockerfile targets Linux arm64. Initialize `vendor/ghostty` first.

```sh
bash experiments/classic/run.sh
```

The script uses uniquely named containers/network, ephemeral pinned SSH keys, no
public ports, credentials/home mounts, privileged mode or Docker socket mount.
It removes its containers/network/keys on exit. Build images/cache are retained.
The original stock protocol rejection test is preserved; the fixture creates its
native pane **before** adapter registration, verifies discovery/control/read,
reconnect/PID continuity, no focus or pane duplication, and remote Core absence
before and after. See `classic-validation.md` for recorded validation results.
