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

#[test]
#[cfg(not(feature = "classic-backend"))]
fn default_binary_cannot_connect_classic_and_does_not_fall_back() {
    let s = Session::new();
    let (_, e) = s.err(&["--json", "machine", "add", "does-not-exist", "--backend", "classic", "--session", "native"]);
    assert_eq!(e["error"]["kind"], "backend_disabled");
    assert_eq!(s.json(&["machine", "ls"])["machines"], json!([]));
    let hello = s.connect().call(1, "hello", json!({}));
    assert!(!hello["result"]["features"].as_array().unwrap().contains(&json!("classic_control")));
}

#[test]
#[cfg(feature = "classic-backend")]
fn classic_is_separate_bounded_and_never_retries_input() {
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
    let (_, e) = s.err(&["--json", "machine", "add", "host", "--backend", "classic"]);
    assert_eq!(e["error"]["kind"], "invalid_params");
    let added = s.json(&["machine", "add", "host", "--backend", "classic", "--session", "native"]);
    assert_eq!(added["id"], "", "no fake Core ID");
    assert_eq!(added["backend"], "classic");
    let attach = s.connect().call(1, "terminal.attach", json!({"terminal":"host/w1:p1", "cols":80, "rows":24}));
    assert_eq!(attach["error"]["data"]["kind"], "unsupported_backend_capability");
    let original = s.json(&["classic", "list", "host"]);
    assert_eq!(
        original,
        json!({"panes":[{"pane_id":"w1:p1", "terminal_id":"term_native", "workspace_id":"w1", "tab_id":"w1:t1"}]})
    );
    let modern = s.json(&["ls"]);
    assert_eq!(modern["terminals"], json!([]));
    assert_eq!(modern["machines"][0]["state"], "on_demand");
    for args in [
        vec!["run", "--on", "host", "--bg", "echo", "no"],
        vec!["read", "host/w1:p1"],
        vec!["machine", "update", "host"],
    ] {
        let (_, e) = s.err(&[vec!["--json"], args].concat());
        assert_eq!(e["error"]["kind"], "unsupported_backend_capability", "{e}");
    }
    let (_, e) = s.err(&["--json", "classic", "send", "host", "w1:p1", "stale", "--text", "no"]);
    assert_eq!(e["error"]["kind"], "classic_stale_identity");
    assert_eq!(s.json(&["classic", "read", "host", "w1:p1", "term_native"])["read"]["text"], "EXISTING");
    let (_, e) =
        s.err(&["--json", "classic", "send", "host", "w1:p1", "term_native", "--text", "literal; $(not-shell)"]);
    assert_eq!(e["error"]["kind"], "classic_outcome_unknown");
    assert_eq!(std::fs::read_to_string(s.root.join("calls")).unwrap().matches("pane.send_input").count(), 1);
    s.json(&["machine", "rename", "host", "native"]);
    s.json(&["server", "restart"]);
    assert_eq!(s.json(&["classic", "list", "native"]), original);
    s.json(&["machine", "rm", "native"]);
}
