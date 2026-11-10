# Classic Herdr sessions

A running classic Herdr session (the one with workspaces, tabs and panes) on
another machine can be added as a machine and talked to over ssh, without
installing anything there. It is a small, separate control surface: see which
panes exist, read their recent text, type into them. It is not a core, and its
panes are not terminals here.

[Back to the README](../README.md) · [Machines](machines.md) ·
[Command reference](commands.md#hc-classic)

## Use

The session must already be running there under a name, and ssh must get in
without asking (`hc machine login` first if it does):

```sh
hc machine add vps --name native --backend classic --session work
hc classic list native
hc classic read native w1:p1 term_RETURNED_BY_LIST --lines 100
hc classic send native w1:p1 term_RETURNED_BY_LIST --text 'printf hello' --key enter
```

`classic list` gives each pane's id, its runtime id, and the workspace and tab
it sits in. Use the pane and runtime ids exactly as returned; they are opaque.
`read` returns recent text (`read.text`), with the pane's workspace/tab,
revision and whether it was truncated. `send` types `--text` literally and
presses each `--key` (native classic key names, repeatable); Enter is never
implied. `accepted: true` means the session took the input, not that a command
ran or finished.

`machine ls` shows a classic machine as `on_demand`: there is no line kept
open; every command is one ssh exchange. `machine login` probes it again.
`machine rename` and `machine rm` only change what this hc knows.

## What it does not do

- Nothing is installed or upgraded there. `--install` and `machine update`
  refuse a classic machine; a session that does not answer with JSON API
  protocol 22 is refused, not upgraded.
- Classic panes are not terminals. `hc ls` leaves them out, and attach, run,
  wait, read, files and anything else addressed as `native/...` answer
  `unsupported_backend_capability`. There is no raw byte stream, no resizing,
  no graphics.
- No panes, tabs or workspaces are created, focused, resized or scrolled.
- No agent status is read from classic. Status in hc comes only from OSC 7501.

## Limits and failure modes

Each exchange has a 20 s deadline and a 1 MiB request/response bound. `read`
asks for 1 to 1000 lines. `send` takes at most 64 KiB of text and 64 keys of up
to 64 bytes each, and needs at least one of the two.

Before `read` and `send`, hc lists the panes again and checks that the pane you
named still runs the runtime you named; a mismatch is `classic_stale_identity`
and nothing is sent. This catches a pane you saw being replaced, but it is not
atomic: a pane replaced between that check and the input still gets it. Do not
automate input into panes that others may be replacing.

Errors are named for what was observed:

- `classic_transport`: ssh or the bridge did not answer; nothing was changed.
- `classic_protocol`: it answered, but not as a classic session hc understands.
- `classic_remote`: the session refused the request (its own error code and
  message are in the text).
- `classic_outcome_unknown`: input was sent but no acknowledgement came back.
  It may have landed. hc never retries input; look at the pane before you do.
