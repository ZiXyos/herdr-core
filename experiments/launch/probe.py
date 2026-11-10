"""Disposable-container-only launch assertions. Deterministic fixture; no paid agents."""
import json
import os
import subprocess
import time

for key in list(os.environ):
    if key.startswith(('HC_', 'HERDR_')):
        del os.environ[key]
os.environ.update(HC_SESSION='launch-test', XDG_STATE_HOME='/var/tmp/launch/state',
                  XDG_RUNTIME_DIR='/var/tmp/launch/run', XDG_CONFIG_HOME='/var/tmp/launch/config')

LAUNCH = 'launch-docker-1'
HASH = 'c0ffee' + 'a' * 58
ARG = 'arg;$(touch /tmp/INJECTED) "q\' end'
ENV_VALUE = 'literal;$(touch /tmp/INJECTED)'


def run(args, data=None, ok=True):
    p = subprocess.run(args, input=data, text=True, capture_output=True, timeout=30)
    if ok:
        assert p.returncode == 0, (args, p.stderr)
    return p


def api(method, params=None):
    p = run(['ssh', 'classic', 'herdr --session compat remote-api-bridge'],
            json.dumps(dict(id='fixture', method=method, params=params or {}))+'\n')
    return json.loads(p.stdout)


def hc(*args):
    return json.loads(run(['hc', '--json', *args]).stdout)


def check(label, condition):
    assert condition, label
    print(json.dumps(dict(assertion=label, passed=True)), flush=True)


def wait_text(pane, terminal, needle):
    for _ in range(50):
        text = hc('classic', 'read', 'native', pane, terminal)['read']['text']
        if needle in text:
            return text
        time.sleep(.1)
    raise AssertionError((needle, text))


def no_core():
    p = run(['ssh', 'classic', 'command -v hc; command -v herdr-core; test -e ~/.local/bin/herdr-core'], ok=False)
    check('remote modern Core absent', p.returncode == 1 and p.stdout == '')


try:
    no_core()
    created = api('workspace.create', {'label': 'launch-fixture'})['result']
    pane, terminal = created['root_pane']['pane_id'], created['root_pane']['terminal_id']
    before = api('pane.list')['result']['panes']
    hc('machine', 'add', 'classic', '--name', 'native', '--backend', 'classic', '--session', 'compat')

    made = hc('classic', 'create', 'native', pane, terminal, '--direction', 'down', '--cwd', '/home/fixture',
              '--env', 'FIXTURE_ENV='+ENV_VALUE, '--launch-id', LAUNCH, '--configuration-hash', HASH)
    new, runtime = made['pane']['pane_id'], made['pane']['terminal_id']
    check('native split returned new opaque identity', new != pane and runtime != terminal)
    check('correlation metadata recorded', made['correlation'] == 'recorded')
    after = api('pane.list')['result']['panes']
    check('exactly one pane added; original identity and focus unchanged',
          len(after) == len(before) + 1
          and [(p['pane_id'], p['terminal_id'], p['focused']) for p in after if p['pane_id'] != new]
          == [(p['pane_id'], p['terminal_id'], p['focused']) for p in before]
          and not next(p for p in after if p['pane_id'] == new)['focused'])
    listed = next(p for p in hc('classic', 'list', 'native')['panes'] if p['pane_id'] == new)
    check('discovery exposes launch correlation tokens',
          listed['tokens'].get('hydra_launch_id') == LAUNCH and listed['tokens'].get('hydra_configuration_hash') == HASH)

    rejected = run(['hc', '--json', 'classic', 'start', 'native', new, 'stale-runtime', '--launch-id', LAUNCH,
                    '--configuration-hash', HASH, '--', 'python3', '/home/fixture/fixture.py', 'x'], ok=False)
    check('stale runtime refuses launch', json.loads(rejected.stderr)['error']['kind'] == 'classic_stale_identity')

    argv = ['python3', '/home/fixture/fixture.py', ARG]
    ack = hc('classic', 'start', 'native', new, runtime, '--cwd', '/home/fixture', '--env', 'FIXTURE_ENV='+ENV_VALUE,
             '--launch-id', LAUNCH, '--configuration-hash', HASH, '--', *argv)
    check('launch input acknowledged', ack['accepted'] is True)
    prestart = ack['prestart']
    check('start returns authoritative prestart shell identity',
          prestart['shell_pid'] == prestart['foreground_process_group_id']
          and prestart['foreground_processes'][0]['pid'] == prestart['shell_pid'])
    for _ in range(50):
        procs = hc('classic', 'inspect', 'native', new, runtime)['process_info']['foreground_processes']
        if any(p.get('argv') == argv for p in procs):
            break
        time.sleep(.1)
    match = [p for p in procs if p.get('argv') == argv]
    check('exact argv observed as foreground process', len(match) == 1)
    check('exact cwd observed', match[0].get('cwd') == '/home/fixture')
    text = wait_text(new, runtime, 'FIXTURE_READY')
    check('env and argv delivered literally', 'env=%s' % ENV_VALUE in text and ARG in text)
    check('launch correlation env delivered', 'FIXTURE_LAUNCH ' + LAUNCH in wait_text(new, runtime, 'FIXTURE_LAUNCH'))
    check('no shell injection executed', run(['ssh', 'classic', 'test -e /tmp/INJECTED'], ok=False).returncode == 1)
    occupied = run(['hc', '--json', 'classic', 'start', 'native', new, runtime, '--launch-id', LAUNCH,
                    '--configuration-hash', HASH, '--', *argv], ok=False)
    check('occupied foreground refuses shell syntax without an input ack',
          json.loads(occupied.stderr)['error']['kind'] == 'classic_shell_not_ready')

    hc('classic', 'send', 'native', new, runtime, '--text', 'ping-42', '--key', 'enter')
    check('interactive fixture controlled via classic input', 'FIXTURE_REPLY ping-42' in wait_text(new, runtime, 'FIXTURE_REPLY ping-42'))
    check('original pane runtime untouched', any(p['pane_id'] == pane and p['terminal_id'] == terminal
                                                 for p in hc('classic', 'list', 'native')['panes']))
    check('no extra panes after launch', len(api('pane.list')['result']['panes']) == len(before) + 1)

    shebang_id = 'launch-docker-shebang'
    shebang_hash = 'b' * 64
    shebang_argv = ['/home/fixture/fixture.py', 'real-shebang']
    made2 = hc('classic', 'create', 'native', pane, terminal, '--cwd', '/home/fixture',
               '--launch-id', shebang_id, '--configuration-hash', shebang_hash)
    new2, runtime2 = made2['pane']['pane_id'], made2['pane']['terminal_id']
    ack2 = hc('classic', 'start', 'native', new2, runtime2, '--cwd', '/home/fixture',
              '--launch-id', shebang_id, '--configuration-hash', shebang_hash, '--', *shebang_argv)
    for _ in range(50):
        info2 = hc('classic', 'inspect', 'native', new2, runtime2)['process_info']
        interpreted = [p for p in info2['foreground_processes'] if p.get('argv', [])[1:] == shebang_argv]
        if interpreted:
            break
        time.sleep(.1)
    check('real shebang fixture exposes interpreter plus script argv', len(interpreted) == 1)
    check('shebang observation differs from requested literal argv', interpreted[0]['argv'] != shebang_argv)
    check('shebang evidence is a new foreground identity',
          interpreted[0]['pid'] not in [p['pid'] for p in ack2['prestart']['foreground_processes']]
          and info2['foreground_process_group_id'] != ack2['prestart']['foreground_process_group_id'])
    check('real shebang fixture executed', 'real-shebang' in wait_text(new2, runtime2, 'FIXTURE_READY'))
    check('no extra panes beyond the two explicit creates', len(api('pane.list')['result']['panes']) == len(before) + 2)

    # ---- Terminal control characters (P1-A), literal delivery, canonical limit ----
    def fresh(label, cwd='/home/fixture', launch='launch-safety', digest='d' * 64):
        root = api('workspace.create', {'label': label})['result']['root_pane']
        made = hc('classic', 'create', 'native', root['pane_id'], root['terminal_id'], '--cwd', cwd,
                  '--launch-id', launch, '--configuration-hash', digest)
        return made['pane']['pane_id'], made['pane']['terminal_id'], made['pane']

    def wait_shell(p, t, name):
        for _ in range(50):
            info = hc('classic', 'inspect', 'native', p, t)['process_info']
            fg = info['foreground_processes']
            if len(fg) == 1 and fg[0]['pid'] == info['shell_pid'] and \
                    fg[0].get('argv', [''])[0].lstrip('-').split('/')[-1] == name:
                return info
            time.sleep(.1)
        raise AssertionError((name, info))

    def start(p, t, argv, env=(), cwd=None, launch='launch-safety', digest='d' * 64, ok=True):
        args = ['hc', '--json', 'classic', 'start', 'native', p, t, '--launch-id', launch, '--configuration-hash', digest]
        for e in env:
            args += ['--env', e]
        if cwd:
            args += ['--cwd', cwd]
        r = run(args + ['--', *argv], ok=ok)
        return json.loads(r.stdout) if ok else json.loads(r.stderr)['error']

    def sentinel(name):
        return run(['ssh', 'classic', 'test -e /tmp/%s' % name], ok=False).returncode == 0

    shells = {}
    for name in ('bash', 'dash'):
        p, t, _ = fresh('safety-' + name)
        # The fixture pane's default shell is /bin/sh (Debian dash); exec the exact shell.
        hc('classic', 'send', 'native', p, t, '--text', 'exec %s -i' % name, '--key', 'enter')
        wait_shell(p, t, name)
        shells[name] = (p, t)
    controls = {'DEL': '\x7f', 'CtrlU': '\x15', 'CtrlC': '\x03', 'CtrlW': '\x17', 'CtrlV': '\x16', 'CtrlD': '\x04',
                'CtrlZ': '\x1a', 'CtrlBackslash': '\x1c', 'Tab': '\t', 'LF': '\n', 'CR': '\r', 'ESC': '\x1b',
                'C1_CSI': '\x9b', 'NEL': '\x85'}
    for name, (p, t) in shells.items():
        screen = hc('classic', 'read', 'native', p, t)['read']['text']
        kinds = set()
        for label, c in controls.items():
            payload = 'x%s; touch /tmp/PWN_%s_%s; #' % (c, name, label)
            kinds.add(start(p, t, ['/bin/echo', payload], ok=False)['kind'])
            kinds.add(start(p, t, ['/bin/echo', 'ok'], env=['FIXTURE_ENV=' + payload], ok=False)['kind'])
            kinds.add(start(p, t, ['/bin/echo', 'ok'], cwd='/tmp/' + payload, ok=False)['kind'])
            kinds.add(start(p, t, ['/bin/echo' + c + 'touch', 'ok'], ok=False)['kind'])
        check('%s: every control payload (argv/env/cwd/executable) rejected as invalid_params' % name, kinds == {'invalid_params'})
        time.sleep(.5)
        check('%s: rejected launches typed nothing (screen unchanged)' % name,
              hc('classic', 'read', 'native', p, t)['read']['text'] == screen)
        check('%s: no control-payload sentinel exists' % name,
              run(['ssh', 'classic', 'ls /tmp | grep -c PWN_%s_' % name], ok=False).stdout.strip() == '0')
        # Baseline: the same quoting typed via the deliberate literal-input API does inject.
        # This proves the payload class is real here and that classic.send stays unrestricted.
        # Herdr delivers text to bash as a bracketed paste: DEL/^U are then inserted literally,
        # but ^C still cancels the paste. dash has no paste mode but the tty flushes typeahead
        # after ^C (ISIG), while ^U (VKILL) erases the whole line including the opening quote.
        control = {'bash': '\x03  ', 'dash': '\x15'}[name]
        hc('classic', 'send', 'native', p, t, '--text', "'/bin/echo' 'x%stouch /tmp/PWN_SEND_%s #'" % (control, name),
           '--key', 'enter')
        for _ in range(30):
            if sentinel('PWN_SEND_' + name):
                break
            time.sleep(.1)
        check('%s: same payload class via literal classic.send executes (baseline, send unrestricted)' % name,
              sentinel('PWN_SEND_' + name))
        wait_shell(p, t, name)

    literal = ['café 日本 😀 \u00a0\u2028x', "$(touch /tmp/INJ) `touch /tmp/INJ` ;|&<>*?[]{}~!# \\ \" '", '!!', '-n']
    for name, (p, t) in shells.items():
        out = '/tmp/argv-%s.json' % name
        ack = start(p, t, ['/home/fixture/argv.py', out, *literal], env=['FIXTURE_ENV=' + literal[1]],
                    cwd='/home/fixture/dir with space é')
        check('%s: benign UTF-8/metacharacter launch acknowledged' % name, ack['accepted'] is True)
        wait_text(p, t, 'ARGV_RECORDED')
        got = json.loads(run(['ssh', 'classic', 'cat ' + out]).stdout)
        check('%s: UTF-8, metacharacters, ! and option-like args delivered literally' % name,
              got == dict(argv=literal, cwd='/home/fixture/dir with space é', env=literal[1]))
        check('%s: no metacharacter injection' % name, not sentinel('INJ'))
        wait_shell(p, t, name)
    p, t = shells['dash']
    screen = hc('classic', 'read', 'native', p, t)['read']['text']
    long_error = start(p, t, ['/home/fixture/argv.py', '/tmp/argv-long-dash.json', 'x' * 1200], ok=False)
    check('dash: line beyond canonical limit refused before input', long_error['kind'] == 'classic_shell_not_ready')
    time.sleep(.5)
    check('dash: refused long launch typed nothing',
          hc('classic', 'read', 'native', p, t)['read']['text'] == screen and not sentinel('argv-long-dash.json'))
    p, t = shells['bash']
    start(p, t, ['/home/fixture/argv.py', '/tmp/argv-long-bash.json', 'y' * 20000])
    for _ in range(50):
        if sentinel('argv-long-bash.json'):
            break
        time.sleep(.1)
    time.sleep(.2)
    check('bash (readline): 20 KB argument delivered exactly',
          json.loads(run(['ssh', 'classic', 'cat /tmp/argv-long-bash.json']).stdout)['argv'] == ['y' * 20000])

    # ---- cwd (P2): relative rejected before any request; symlink resolved by the host ----
    panes_before = len(api('pane.list')['result']['panes'])
    rel = run(['hc', '--json', 'classic', 'create', 'native', pane, terminal, '--cwd', 'fixture',
               '--launch-id', 'launch-rel', '--configuration-hash', 'e' * 64], ok=False)
    check('relative cwd refused as invalid_params with no pane created',
          json.loads(rel.stderr)['error']['kind'] == 'invalid_params'
          and len(api('pane.list')['result']['panes']) == panes_before)
    _, _, linked = fresh('symlink-cwd', cwd='/home/fixture/link', launch='launch-symlink', digest='e' * 64)
    check('symlink cwd: native pane reports the resolved path', linked.get('cwd') == '/home/fixture/real')
    print('RECORD ' + json.dumps(dict(case='symlink_cwd', requested='/home/fixture/link', pane=linked)), flush=True)

    # ---- process.title rewrite (P1-B) and the optional readiness handshake ----
    def title_case(label, launch, digest, extra):
        p, t, created = fresh(label, launch=launch, digest=digest)
        wait_shell(p, t, 'sh')
        argv = ['/home/fixture/title.js', *extra, '--session', '/tmp/session.jsonl', 'Initial instructions; $(not shell)']
        ack = start(p, t, argv, cwd='/home/fixture', launch=launch, digest=digest)
        # The fixture prints only after process.title was assigned, so inspection below is
        # deterministically after the rewrite (not a timing race).
        wait_text(p, t, 'READY_REPORTED ok' if '--ready' in extra else 'TITLE_SET')
        info = hc('classic', 'inspect', 'native', p, t)['process_info']
        listed = next(x for x in hc('classic', 'list', 'native')['panes'] if x['pane_id'] == p)
        new = [x for x in info['foreground_processes']
               if x['pid'] not in [y['pid'] for y in ack['prestart']['foreground_processes']]]
        check('%s: one new foreground process in a new process group' % label,
              len(new) == 1 and info['foreground_process_group_id'] != ack['prestart']['foreground_process_group_id'])
        check('%s: title rewrite hides requested argv (observed %r)' % (label, new[0].get('argv')),
              new[0].get('argv') != argv and (new[0].get('argv') or [])[-len(argv) + 1:] != argv[1:]
              and 'Initial instructions; $(not shell)' not in json.dumps(new[0]))
        print('RECORD ' + json.dumps(dict(case=label, launch_id=launch, configuration_hash=digest, argv=argv,
                                          cwd='/home/fixture', created=created, prestart=ack['prestart'],
                                          inspected=info, listed=listed)), flush=True)
        return new[0], listed
    _, listed = title_case('title-rewrite', 'launch-title', '1' * 64, [])
    check('title rewrite without handshake: no readiness tokens', 'hydra_ready_pid' not in listed.get('tokens', {}))
    proc, listed = title_case('title-rewrite-ready', 'launch-title-ready', '2' * 64, ['--ready'])
    check('handshake binds launch ID, hash and the new foreground PID',
          listed['tokens'].get('hydra_ready_launch_id') == 'launch-title-ready'
          and listed['tokens'].get('hydra_ready_configuration_hash') == '2' * 64
          and listed['tokens'].get('hydra_ready_pid') == str(proc['pid'])
          and listed['tokens'].get('hydra_launch_id') == 'launch-title-ready')
    no_core()
    print(json.dumps(dict(original=[pane, terminal], created=[new, runtime], process=match[0],
                          shebang=[new2, runtime2], interpreted=interpreted[0])), flush=True)
finally:
    run(['hc', 'server', 'stop'], ok=False)
