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

#[test]
fn classic_create_start_and_inspect_preserve_native_identity() {
    let mut s = Session::new();
    let bridge = s.root.join("classic-ssh");
    std::fs::write(&bridge, format!(r#"#!/usr/bin/env python3
import json, os, sys
assert sys.argv[2] == 'herdr --session native remote-api-bridge', sys.argv
r = json.loads(sys.stdin.readline())
with open('{root}/requests', 'a') as f: f.write(json.dumps(r)+'\n')
p1 = dict(pane_id='w1:p1', terminal_id='term_1', workspace_id='w1', tab_id='w1:t1')
p2 = dict(pane_id='w1:p2', terminal_id='term_2', workspace_id='w1', tab_id='w1:t1', cwd='/tmp/work')
if os.path.exists('{root}/metadata'): p2['tokens'] = dict(hydra_launch_id='launch-1', hydra_configuration_hash='a'*64)
if r['method'] == 'ping':
    result = dict(type='pong', protocol=22, version='0.9.5')
elif r['method'] == 'pane.list':
    result = dict(panes=[p1]+([p2] if os.path.exists('{root}/created') else []))
elif r['method'] == 'pane.split':
    assert r['params']['focus'] is False
    assert r['params']['env']['HYDRA_LAUNCH_ID'] == 'launch-1'
    assert r['params']['env']['FIXTURE'] == 'literal;$(not-shell)'
    open('{root}/created', 'w').close()
    result = dict(type='pane_info', pane=p2)
elif r['method'] == 'pane.report_metadata':
    assert r['params']['tokens']['hydra_configuration_hash'] == 'a'*64
    open('{root}/metadata', 'w').close()
    result = dict(type='ok')
elif r['method'] == 'pane.send_input':
    assert r['params']['pane_id'] == 'w1:p2'
    assert r['params']['keys'] == ['Enter']
    open('{root}/launch', 'w').write(r['params']['text'])
    result = dict(type='ok')
elif r['method'] == 'pane.process_info':
    if os.path.exists('{root}/launch'):
        info = dict(pane_id='w1:p2', shell_pid=10, foreground_process_group_id=11,
            foreground_processes=[dict(pid=11, name='python3', argv=['python3','/tmp/fixture.py','literal;$(not-shell)'], cwd='/tmp/work')])
    else:
        info = dict(pane_id='w1:p2', shell_pid=10, foreground_process_group_id=10,
            foreground_processes=[dict(pid=10, name='sh', argv=['/bin/sh'], cwd='/tmp/work')])
    result = dict(type='pane_process_info', process_info=info)
else:
    raise AssertionError(r)
print(json.dumps(dict(id=r['id'], result=result)), flush=True)
"#, root=s.root.display())).unwrap();
    std::fs::set_permissions(&bridge, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push(("HC_SSH".into(), bridge.display().to_string()));
    s.json(&["machine", "add", "host", "--name", "native", "--backend", "classic", "--session", "native"]);
    let created = s.json(&[
        "classic",
        "create",
        "native",
        "w1:p1",
        "term_1",
        "--direction",
        "down",
        "--cwd",
        "/tmp/work",
        "--env",
        "FIXTURE=literal;$(not-shell)",
        "--launch-id",
        "launch-1",
        "--configuration-hash",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ]);
    assert_eq!(created["pane"]["pane_id"], "w1:p2");
    assert_eq!(created["pane"]["terminal_id"], "term_2");
    assert_eq!(created["correlation"], "recorded");
    assert!(s.json(&["classic", "list", "native"])["panes"][1]["tokens"]["hydra_launch_id"] == "launch-1");
    let (_, stale) = s.err(&[
        "classic",
        "start",
        "native",
        "w1:p2",
        "reused",
        "--launch-id",
        "launch-1",
        "--configuration-hash",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "python3",
        "/tmp/fixture.py",
    ]);
    assert_eq!(stale["error"]["kind"], "classic_stale_identity");
    assert!(!s.root.join("launch").exists());
    // Terminal control characters and relative cwd fail before any native request.
    let requests_before = std::fs::read_to_string(s.root.join("requests")).unwrap();
    let hash = "a".repeat(64);
    for (arg, env, cwd) in [
        ("\u{7f}; touch /tmp/PWN; #", "FIXTURE=ok", "/tmp/work"),
        ("x\u{15}touch /tmp/PWN #", "FIXTURE=ok", "/tmp/work"),
        ("x\u{3}touch /tmp/PWN #", "FIXTURE=ok", "/tmp/work"),
        ("x\ttouch", "FIXTURE=ok", "/tmp/work"),
        ("x\ntouch /tmp/PWN", "FIXTURE=ok", "/tmp/work"),
        ("ok", "FIXTURE=\u{1b}[200~", "/tmp/work"),
        ("ok", "FIXTURE=ok", "/tmp/w\u{9b}x"),
        ("ok", "FIXTURE=ok", "relative/dir"),
    ] {
        // create carries only cwd/env; argv-only payloads are checked on start.
        for method in if arg == "ok" { &["create", "start"][..] } else { &["start"][..] } {
            let method = *method;
            let mut args = vec!["classic", method, "native"];
            args.extend(if method == "create" { ["w1:p1", "term_1"] } else { ["w1:p2", "term_2"] });
            args.extend(["--cwd", cwd, "--env", env, "--launch-id", "launch-1", "--configuration-hash", &hash]);
            if method == "start" {
                args.extend(["--", "/bin/echo", arg]);
            }
            let (_, rejected) = s.err(&args);
            assert_eq!(rejected["error"]["kind"], "invalid_params", "{method} {arg:?} {env:?} {cwd:?}");
        }
    }
    assert_eq!(
        std::fs::read_to_string(s.root.join("requests")).unwrap(),
        requests_before,
        "rejected launch configuration reaches no native request"
    );
    assert!(!s.root.join("launch").exists());
    let started = s.json(&[
        "classic",
        "start",
        "native",
        "w1:p2",
        "term_2",
        "--cwd",
        "/tmp/work",
        "--launch-id",
        "launch-1",
        "--configuration-hash",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "python3",
        "/tmp/fixture.py",
        "literal;$(not-shell)",
    ]);
    assert_eq!(started["accepted"], true);
    assert_eq!(started["prestart"]["shell_pid"], 10);
    assert_eq!(started["prestart"]["foreground_processes"][0]["argv"], json!(["/bin/sh"]));
    let command = std::fs::read_to_string(s.root.join("launch")).unwrap();
    assert!(command.contains("'python3' '/tmp/fixture.py' 'literal;$(not-shell)'"));
    let inspected = s.json(&["classic", "inspect", "native", "w1:p2", "term_2"]);
    assert_eq!(inspected["process_info"]["foreground_processes"][0]["argv"][0], "python3");
    assert_eq!(inspected["process_info"]["foreground_processes"][0]["cwd"], "/tmp/work");
    let before = std::fs::read_to_string(s.root.join("requests")).unwrap().matches("pane.send_input").count();
    let (_, occupied) = s.err(&[
        "classic",
        "start",
        "native",
        "w1:p2",
        "term_2",
        "--launch-id",
        "launch-1",
        "--configuration-hash",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "python3",
        "/tmp/fixture.py",
    ]);
    assert_eq!(occupied["error"]["kind"], "classic_shell_not_ready");
    let after = std::fs::read_to_string(s.root.join("requests")).unwrap().matches("pane.send_input").count();
    assert_eq!(before, after, "occupied foreground receives no launch syntax");
}

#[test]
fn classic_create_ack_loss_is_unknown_and_metadata_loss_keeps_created_identity() {
    for lose_split in [true, false] {
        let mut s = Session::new();
        let bridge = s.root.join("classic-ssh");
        std::fs::write(
            &bridge,
            format!(
                r#"#!/usr/bin/env python3
import json, os, sys
r = json.loads(sys.stdin.readline())
with open('{root}/calls', 'a') as f: f.write(r['method']+'\n')
p1 = dict(pane_id='w1:p1', terminal_id='term_1', workspace_id='w1', tab_id='w1:t1')
p2 = dict(pane_id='w1:p2', terminal_id='term_2', workspace_id='w1', tab_id='w1:t1')
if r['method'] == 'ping': result = dict(type='pong', protocol=22, version='0.9.5')
elif r['method'] == 'pane.list': result = dict(panes=[p1]+([p2] if os.path.exists('{root}/created') else []))
elif r['method'] == 'pane.split':
    open('{root}/created', 'w').close()
    if {lose_split}: sys.exit(0)
    result = dict(type='pane_info', pane=p2)
elif r['method'] == 'pane.report_metadata': sys.exit(0)
else: raise AssertionError(r)
print(json.dumps(dict(id=r['id'], result=result)), flush=True)
"#,
                root = s.root.display(),
                lose_split = if lose_split { "True" } else { "False" }
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bridge, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        s.env.push(("HC_SSH".into(), bridge.display().to_string()));
        s.json(&["machine", "add", "host", "--name", "native", "--backend", "classic", "--session", "native"]);
        let args = [
            "classic",
            "create",
            "native",
            "w1:p1",
            "term_1",
            "--launch-id",
            "launch-1",
            "--configuration-hash",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ];
        if lose_split {
            let (_, e) = s.err(&args);
            assert_eq!(e["error"]["kind"], "classic_outcome_unknown");
        } else {
            let created = s.json(&args);
            assert_eq!(created["correlation"], "unknown");
            assert_eq!(created["pane"]["pane_id"], "w1:p2");
        }
        let calls = std::fs::read_to_string(s.root.join("calls")).unwrap();
        assert_eq!(calls.matches("pane.split").count(), 1);
        assert_eq!(calls.matches("pane.report_metadata").count(), if lose_split { 0 } else { 1 });
    }
}
