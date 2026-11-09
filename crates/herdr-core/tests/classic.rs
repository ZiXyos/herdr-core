#![cfg(unix)]

mod common;
use common::*;
use serde_json::json;

#[test]
fn profiles_default_to_modern() {
    let m: herdr_core::machines::Machine = serde_json::from_value(json!({
        "name":"old", "target":"host", "session":null,
        "id":"permanent", "host":"host", "version":"0.1.18"
    }))
    .unwrap();
    assert_eq!(m.backend, herdr_core::machines::Backend::Modern);
    let params: herdr_core::api::MachineAddParams = serde_json::from_value(json!({"target":"host"})).unwrap();
    assert_eq!(params.backend, herdr_core::machines::Backend::Modern);
}

/// A session whose `ssh` is a fake classic 0.9.5 bridge: one pane, a read that answers,
/// and a send that deliberately dies without acknowledging. Every method call is logged to `calls`.
fn classic_host() -> Session {
    let mut s = Session::new();
    let bridge = s.root.join("classic-ssh");
    std::fs::write(&bridge, format!(r#"#!/usr/bin/env python3
import json, sys
assert sys.argv[2] == 'herdr --session native remote-api-bridge', sys.argv
r = json.loads(sys.stdin.readline())
with open('{root}/calls', 'a') as f: f.write(r['method']+'\n')
if r['method'] == 'ping':
    result = dict(type='pong', protocol=22, version='0.9.5')
elif r['method'] == 'pane.list':
    result = dict(panes=[dict(pane_id='w1:p1', terminal_id='term_native', workspace_id='w1', tab_id='w1:t1', agent_status='working')])
elif r['method'] == 'pane.read':
    result = dict(type='pane_read', read=dict(pane_id='w1:p1', workspace_id='w1', tab_id='w1:t1', text='EXISTING', revision=0, truncated=False))
elif r['method'] == 'pane.send_input':
    assert r['params']['text'] == 'literal; $(not-shell)'
    sys.exit(0) # input might have happened; deliberately lose its acknowledgement
else:
    raise AssertionError(r)
print(json.dumps(dict(id=r['id'], result=result)), flush=True)
"#, root=s.root.display())).unwrap();
    std::fs::set_permissions(&bridge, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push(("HC_SSH".into(), bridge.display().to_string()));
    s
}

#[test]
fn a_classic_machine_is_not_a_core_and_refuses_core_operations() {
    let s = classic_host();
    let (_, e) = s.err(&["--json", "machine", "add", "host", "--backend", "classic"]);
    assert_eq!(e["error"]["kind"], "invalid_params", "a classic session must be named");
    let added = s.json(&["machine", "add", "host", "--backend", "classic", "--session", "native"]);
    assert_eq!(added["id"], "", "no fake Core ID");
    assert_eq!(added["backend"], "classic");
    assert_eq!(added["version"], "0.9.5");
    let hello = s.connect().call(1, "hello", json!({}));
    assert!(hello["result"]["features"].as_array().unwrap().contains(&json!("classic_control")));
    let modern = s.json(&["ls"]);
    assert_eq!(modern["terminals"], json!([]), "classic panes are not Core terminals");
    assert_eq!(modern["machines"][0]["state"], "on_demand");
    let attach = s.connect().call(1, "terminal.attach", json!({"terminal":"host/w1:p1", "cols":80, "rows":24}));
    assert_eq!(attach["error"]["data"]["kind"], "unsupported_backend_capability");
    for args in [
        vec!["run", "--on", "host", "--bg", "echo", "no"],
        vec!["read", "host/w1:p1"],
        vec!["machine", "update", "host"],
    ] {
        let (_, e) = s.err(&[vec!["--json"], args].concat());
        assert_eq!(e["error"]["kind"], "unsupported_backend_capability", "{e}");
    }
    s.json(&["machine", "rm", "host"]);
}

#[test]
fn classic_discovery_read_and_input_are_bounded_and_never_retried() {
    let s = classic_host();
    s.json(&["machine", "add", "host", "--backend", "classic", "--session", "native"]);
    let original = s.json(&["classic", "list", "host"]);
    assert_eq!(
        original,
        json!({"panes":[{"pane_id":"w1:p1", "terminal_id":"term_native", "workspace_id":"w1", "tab_id":"w1:t1"}]}),
        "opaque ids kept, vendor agent status dropped"
    );
    let (_, e) = s.err(&["--json", "classic", "send", "host", "w1:p1", "term_native"]);
    assert_eq!(e["error"]["kind"], "invalid_params", "nothing to send");
    let (_, e) = s.err(&["--json", "classic", "send", "host", "w1:p1", "stale", "--text", "no"]);
    assert_eq!(e["error"]["kind"], "classic_stale_identity");
    assert_eq!(s.json(&["classic", "read", "host", "w1:p1", "term_native"])["read"]["text"], "EXISTING");
    let (_, e) =
        s.err(&["--json", "classic", "send", "host", "w1:p1", "term_native", "--text", "literal; $(not-shell)"]);
    assert_eq!(e["error"]["kind"], "classic_outcome_unknown");
    let calls = std::fs::read_to_string(s.root.join("calls")).unwrap();
    assert_eq!(calls.matches("pane.send_input").count(), 1, "a lost acknowledgement is never retried");
    assert_eq!(calls.matches("ping").count(), 1, "probed once, at add; not before every call");
    s.json(&["machine", "rename", "host", "native"]);
    s.json(&["server", "restart"]);
    assert_eq!(s.json(&["classic", "list", "native"]), original, "a saved profile survives a rename and a restart");
    s.json(&["machine", "rm", "native"]);
}
