"""Disposable-container-only interoperability assertions. No paid agents."""
import json
import os
import subprocess
import time

for key in list(os.environ):
    if key.startswith(('HC_', 'HERDR_')):
        del os.environ[key]
os.environ.update(HC_SESSION='adapter-test', XDG_STATE_HOME='/var/tmp/adapter/state',
                  XDG_RUNTIME_DIR='/var/tmp/adapter/run', XDG_CONFIG_HOME='/var/tmp/adapter/config')


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


def rejected(kind, *args):
    p = run(['hc', '--json', *args], ok=False)
    check('reject '+ ' '.join(args), p.returncode != 0 and json.loads(p.stderr)['error']['kind'] == kind)


def no_core():
    p = run(['ssh', 'classic', 'command -v hc; command -v herdr-core; test -e ~/.local/bin/herdr-core'], ok=False)
    check('remote modern Core absent', p.returncode == 1 and p.stdout == '')


try:
    no_core()
    # Deliberate native setup predates registration; adapter must reuse this pane.
    created = api('workspace.create', {'label':'original-fixture'})['result']
    pane = created['root_pane']['pane_id']
    terminal = created['root_pane']['terminal_id']
    before = api('pane.process_info', {'pane_id':pane})['result']
    focus_before = api('pane.list')['result']['panes']
    check('original protocol remains incompatible', 'error' in api('hello', {'framed':True}))
    rejected('hc_missing', 'machine', 'add', 'classic', '--name', 'stock')
    m = hc('machine', 'add', 'classic', '--name', 'native', '--backend', 'classic', '--session', 'compat')
    check('explicit backend, session, no fabricated Core ID', m['backend'] == 'classic' and m['session'] == 'compat' and m['id'] == '')
    listed = hc('classic', 'list', 'native')['panes']
    check('original opaque pane/runtime discovered', any(p['pane_id'] == pane and p['terminal_id'] == terminal for p in listed))
    check('native topology associations preserved', listed[0]['workspace_id'] == created['workspace']['workspace_id'] and listed[0]['tab_id'] == created['tab']['tab_id'])
    check('no vendor agent status projected', all('agent_status' not in p for p in listed))
    check('classic panes not modern terminal holders', hc('ls')['terminals'] == [])
    rejected('unsupported_backend_capability', 'read', 'native/'+pane)
    rejected('unsupported_backend_capability', 'run', '--on', 'native', '--bg', 'echo', 'no')
    rejected('unsupported_backend_capability', 'machine', 'update', 'native')
    rejected('classic_stale_identity', 'classic', 'send', 'native', pane, 'stale', '--text', 'NO')
    ack = hc('classic', 'send', 'native', pane, terminal, '--text', "printf 'ADAPTER_%s pid=%s\\n' VERIFIED $$", '--key', 'enter')
    check('native input acknowledged', ack['accepted'] is True)
    for _ in range(30):
        read = hc('classic', 'read', 'native', pane, terminal)
        if 'ADAPTER_VERIFIED pid=' in read['read']['text']:
            break
        time.sleep(.1)
    check('deterministic command output read from original pane', 'ADAPTER_VERIFIED pid=' in read['read']['text'])
    hc('server', 'restart')
    check('reconnect reuses original identity', hc('classic', 'list', 'native')['panes'] == listed)
    check('original shell process preserved', api('pane.process_info', {'pane_id':pane})['result'] == before)
    after = api('pane.list')['result']['panes']
    check('no pane duplication or focus mutation', len(after) == len(focus_before) and [(p['pane_id'], p['focused']) for p in after] == [(p['pane_id'], p['focused']) for p in focus_before])
    no_core()
    print(json.dumps(dict(original_pane=pane, original_runtime=terminal, process=before)), flush=True)
finally:
    run(['hc', 'server', 'stop'], ok=False)
