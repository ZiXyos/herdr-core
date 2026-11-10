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

## Create a pane and start a program

```sh
hc classic create native w1:p1 term_RETURNED_BY_LIST --direction down --cwd /work \
  --env KEY=value --launch-id launch-1 --configuration-hash <64 lowercase hex>
hc classic start native w1:p2 term_RETURNED_BY_CREATE --cwd /work --env KEY=value \
  --launch-id launch-1 --configuration-hash <same> -- program arg1 'arg 2'
hc classic inspect native w1:p2 term_RETURNED_BY_CREATE
```

`create` checks the named pane/runtime, splits it with `focus: false`, cwd and
env (plus `HYDRA_LAUNCH_ID` and `HYDRA_CONFIGURATION_HASH`), then tags the new
pane with the `hydra_launch_id` / `hydra_configuration_hash` metadata tokens
that `classic list` shows. A lost split reply is `classic_outcome_unknown`; a
lost or refused tag keeps the confirmed pane and reports `correlation:
unknown|unavailable`. Correlation is evidence for reconciling, not idempotency.

`start` checks every value before anything is sent: an executable, at most 128
arguments, POSIX env names, the two reserved keys, 64 KiB in all, an absolute
`cwd`, and **no control characters** in any argument, env value or cwd (C0
including tab, newline and CR, DEL, and C1). `create` applies the same cwd/env
rules before its first request. Control characters are refused rather than
quoted because the command is *typed*: the tty and the shell's line editor act
on them (erase, kill line, interrupt, completion, submit) before the shell sees
any quote. Example: `'x<^U>touch /tmp/f #'` runs `touch` in dash, and in bash a
`^C` cancels Herdr's bracketed paste and the rest runs. `classic send` is the
deliberate literal-input path and is not restricted this way.

Then it asks the pane for its foreground processes. It types nothing unless
exactly one foreground process is the pane's own shell, leading its process
group, with an argv of an interactive `sh`, `bash`, `dash`, `ash`, `zsh`, `ksh`,
`mksh` or `pdksh` (no script operand, `-c`, `-s` or long option). Anything else,
an unknown argv included, is `classic_shell_not_ready`. Shells other than bash
and zsh may read through the tty's canonical buffer, which truncates a line at
1024 bytes on macOS and 4095 on Linux and leaves an open quote. For them a
command of 1000 bytes or more is also `classic_shell_not_ready`.

Then it types `cd -- 'cwd' && KEY='v' 'argv0' 'arg' ...` with Enter. Single
quotes keep shell metacharacters (`$`, backquotes, `;`, `!`, globs) and UTF-8
literal. This assumes default line editing: user `bind` macros, `stty` special
characters moved to printable keys, or a shell with editing switched off at run
time are not detected. The reply is `accepted: true` plus `prestart`, the
foreground identity seen before typing. Acceptance is not proof the program
runs: compare `inspect` with `prestart` and require a new foreground PID and
process group.

The readiness check and the input are two requests, so a foreground that
changes in between still gets the text. Readiness also cannot see the line
buffer. Half-typed input, a continuation (PS2) prompt, or typeahead that a
still-running rc file or prompt hook consumes all look like an idle prompt, and
the launch is appended to or swallowed by them. In that case the outcome is
whatever the shell makes of the combined line, so treat acceptance as unknown
until `inspect` shows the new process.

The host resolves `cwd`: a symlink becomes its target, and a missing directory
silently falls back to another directory (verified on Herdr 0.9.5). The pane
`cwd` returned by `create` and the process `cwd` from `inspect` are those
resolved paths. They can only equal what was requested when the request was
already canonical.

A program can rewrite its own argv. On Linux, Node's `process.title` does this,
and Pi sets it at startup. After that, `inspect` shows only the title, so argv
alone cannot confirm such a launch. A program may report its own readiness
instead by setting pane metadata tokens through `HERDR_SOCKET_PATH` /
`HERDR_PANE_ID`. hc passes all tokens through `classic list` unchanged and
interprets none of them.
`inspect` only looks.
Nothing here is specific to Pi or any other program.

## What it does not do

- Nothing is installed or upgraded there. `--install` and `machine update`
  refuse a classic machine; a session that does not answer with JSON API
  protocol 22 is refused, not upgraded.
- Classic panes are not terminals. `hc ls` leaves them out, and attach, run,
  wait, read, files and anything else addressed as `native/...` answer
  `unsupported_backend_capability`. There is no raw byte stream, no resizing,
  no graphics.
- Only `classic create` makes a pane: an unfocused split of a pane you name.
  No tabs or workspaces are created, and nothing is focused, resized or
  scrolled. There is no `layout.apply`.
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
