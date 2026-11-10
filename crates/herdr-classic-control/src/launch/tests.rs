use super::*;
use std::sync::Mutex;

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn hash() -> String {
    "a".repeat(64)
}

fn launch<'a>(argv: &'a [String], cwd: Option<&'a str>, env: &'a BTreeMap<String, String>, h: &'a str) -> Launch<'a> {
    Launch { argv, cwd, env, launch_id: "launch-1", configuration_hash: h }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

/// Scripted transport: records every request and answers from a queue (None = lost reply).
struct Scripted {
    requests: Mutex<Vec<(String, Value)>>,
    replies: Mutex<Vec<Option<Result<Value, ControlError>>>>,
}

impl Scripted {
    fn new(replies: Vec<Option<Result<Value, ControlError>>>) -> Self {
        Self { requests: Mutex::new(Vec::new()), replies: Mutex::new(replies.into_iter().rev().collect()) }
    }

    fn methods(&self) -> Vec<String> {
        self.requests.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
    }
}

impl ControlTransport for Scripted {
    async fn exchange(&self, _session: &str, request: Request) -> Result<Value, ControlError> {
        self.requests.lock().unwrap().push((request.method.to_string(), request.params.clone()));
        match self.replies.lock().unwrap().pop().flatten() {
            Some(reply) => reply,
            None => Err(lost(request.mutation)),
        }
    }
}

fn panes() -> Value {
    json!({"panes":[{"pane_id":"w1:p2","terminal_id":"term_2","workspace_id":"w1","tab_id":"w1:t1"}]})
}

fn shell_info(argv: &[&str]) -> Value {
    json!({"type":"pane_process_info","process_info":{"pane_id":"w1:p2","shell_pid":10,
        "foreground_process_group_id":10,"foreground_processes":[{"pid":10,"name":"sh","argv":argv}]}})
}

#[test]
fn launch_serialization_quotes_shell_metacharacters_exactly() {
    let argv = strings(&["python3", "-c", "print('ok')", "; touch /tmp/no; $(touch /tmp/no) \" '"]);
    let env = env(&[("FIXTURE_VALUE", "x;$(touch /tmp/no) \" '")]);
    let h = hash();
    let command = launch_command(&launch(&argv, Some("/tmp/work; touch /tmp/no"), &env, &h)).unwrap();
    assert_eq!(
        command,
        "cd -- '/tmp/work; touch /tmp/no' && FIXTURE_VALUE='x;$(touch /tmp/no) \" '\\''' HYDRA_CONFIGURATION_HASH='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' HYDRA_LAUNCH_ID='launch-1' 'python3' '-c' 'print('\\''ok'\\'')' '; touch /tmp/no; $(touch /tmp/no) \" '\\'''"
    );
    let bad = self::env(&[("BAD=KEY", "x")]);
    assert_eq!(launch_command(&launch(&argv, None, &bad, &h)).unwrap_err().kind, "invalid_params");
}

#[test]
fn correlation_and_configuration_are_validated_before_effects() {
    let argv = strings(&["pi"]);
    let empty = BTreeMap::new();
    let h = hash();
    let mut bad_id = launch(&argv, None, &empty, &h);
    bad_id.launch_id = "bad id";
    assert_eq!(launch_command(&bad_id).unwrap_err().kind, "invalid_params");
    assert_eq!(launch_command(&launch(&argv, None, &empty, "not-a-hash")).unwrap_err().kind, "invalid_params");
    assert_eq!(launch_command(&launch(&[], None, &empty, &h)).unwrap_err().kind, "invalid_params");
    assert_eq!(launch_command(&launch(&argv, Some("bad\0path"), &empty, &h)).unwrap_err().kind, "invalid_params");
    let reserved = env(&[("HYDRA_LAUNCH_ID", "other")]);
    assert_eq!(launch_command(&launch(&argv, None, &reserved, &h)).unwrap_err().kind, "invalid_params");
}

#[test]
fn terminal_control_characters_are_rejected_before_any_request() {
    let h = hash();
    // Every C0 control, DEL and every C1 control (as UTF-8) in every typed field.
    for control in (0u32..0x20).chain(0x7f..0xa0).map(|c| char::from_u32(c).unwrap()) {
        let value = format!("x{control}; touch /tmp/PWN #");
        let ok_env = env(&[("V", "ok")]);
        let bad_env = env(&[("V", &value)]);
        let ok_argv = strings(&["/bin/echo", "ok"]);
        let bad_argv = strings(&["/bin/echo", &value]);
        let bad_exe = vec![format!("/bin/echo{control}")];
        let bad_cwd = format!("/{value}");
        for (argv, cwd, env) in [
            (&bad_argv, "/tmp", &ok_env),
            (&ok_argv, "/tmp", &bad_env),
            (&ok_argv, bad_cwd.as_str(), &ok_env),
            (&bad_exe, "/tmp", &ok_env),
        ] {
            let error = launch_command(&launch(argv, Some(cwd), env, &h)).unwrap_err();
            assert_eq!(error.kind, "invalid_params", "U+{:04X}", control as u32);
            assert!(error.message.contains("control characters"), "{}", error.message);
        }
    }
    // Benign UTF-8 and shell metacharacters stay literal (single-quoted, unchanged).
    let literal = "café 日本 😀 \u{a0}\u{2028} $(id) `id` ;|&<>*?[]{}~!\\ \" '";
    let argv = strings(&["/bin/echo", literal]);
    let env = env(&[("V", literal)]);
    let command = launch_command(&launch(&argv, Some("/tmp"), &env, &h)).unwrap();
    assert!(command.contains(&shell_quote(literal)));
    assert!(!command.chars().any(char::is_control));
}

#[test]
fn cwd_must_be_absolute() {
    for cwd in ["relative", "./x", "~/x", "", " /tmp"] {
        assert_eq!(launch_cwd(Some(cwd)).unwrap_err().kind, "invalid_params", "{cwd:?}");
    }
    assert_eq!(launch_cwd(Some("/tmp/x y")).unwrap(), Some("/tmp/x y"));
    assert_eq!(launch_cwd(None).unwrap(), None);
}

fn info(argv: &[&str]) -> ProcessInfo {
    serde_json::from_value(shell_info(argv)["process_info"].clone()).unwrap()
}

#[test]
fn canonical_mode_shells_refuse_lines_they_would_truncate() {
    let long = "x".repeat(CANONICAL_LINE_LIMIT);
    for name in ["sh", "dash", "ash", "ksh", "mksh"] {
        let shell = info(&[&format!("-{name}")]);
        assert!(canonical_line_fits(&shell, &long[1..]).is_ok(), "{name}");
        assert_eq!(canonical_line_fits(&shell, &long).unwrap_err().kind, "classic_shell_not_ready", "{name}");
    }
    for name in ["bash", "zsh"] {
        assert!(canonical_line_fits(&info(&[name]), &"x".repeat(65536)).is_ok(), "{name}");
    }
}

#[test]
fn start_readiness_requires_an_interactive_posix_shell() {
    for argv in [vec!["/bin/sh"], vec!["bash", "-li"], vec!["-zsh"], vec!["dash", "--"]] {
        assert!(interactive_posix_shell(&info(&argv)), "{argv:?}");
    }
    for argv in [
        vec!["fish"],
        vec!["sh", "-c", "echo occupied"],
        vec!["sh", "script.sh"],
        vec!["sh", "-s"],
        vec!["zsh", "--sh-in-stdin"],
        vec!["vim"],
    ] {
        assert!(!interactive_posix_shell(&info(&argv)), "{argv:?}");
    }
    let mut occupied = info(&["python3", "fixture.py"]);
    occupied.foreground_processes[0].pid = 11;
    occupied.foreground_process_group_id = Some(11);
    assert!(!interactive_posix_shell(&occupied));
    let mut unknown = info(&["sh"]);
    unknown.foreground_processes[0].argv = None;
    assert!(!interactive_posix_shell(&unknown));
    unknown.foreground_processes.push(unknown.foreground_processes[0].clone());
    assert!(!interactive_posix_shell(&unknown));
}

#[tokio::test]
async fn start_sends_nothing_unless_valid_and_ready_and_never_retries() {
    let argv = strings(&["/bin/echo", "ok"]);
    let empty = BTreeMap::new();
    let h = hash();
    // Invalid configuration: no request at all.
    let t = Scripted::new(vec![]);
    let bad = strings(&["/bin/echo", "x\u{15}y"]);
    assert_eq!(
        start(&t, "work", "w1:p2", "term_2", &launch(&bad, None, &empty, &h)).await.unwrap_err().kind,
        "invalid_params"
    );
    assert!(t.methods().is_empty());
    // Occupied foreground: inspected, never typed into.
    let t = Scripted::new(vec![Some(Ok(panes())), Some(Ok(shell_info(&["sh", "-c", "x"])))]);
    let error = start(&t, "work", "w1:p2", "term_2", &launch(&argv, None, &empty, &h)).await.unwrap_err();
    assert_eq!(error.kind, "classic_shell_not_ready");
    assert_eq!(t.methods(), ["pane.list", "pane.process_info"]);
    // Lost input reply: outcome unknown, exactly one send, prestart not fabricated.
    let t = Scripted::new(vec![Some(Ok(panes())), Some(Ok(shell_info(&["bash"]))), Some(Ok(panes())), None]);
    let error = start(&t, "work", "w1:p2", "term_2", &launch(&argv, None, &empty, &h)).await.unwrap_err();
    assert_eq!(error.kind, "classic_outcome_unknown");
    assert_eq!(t.methods(), ["pane.list", "pane.process_info", "pane.list", "pane.send_input"]);
    // Accepted: reports the prestart identity observed before typing.
    let ok = json!({"type":"ok"});
    let t = Scripted::new(vec![Some(Ok(panes())), Some(Ok(shell_info(&["bash"]))), Some(Ok(panes())), Some(Ok(ok))]);
    let started = start(&t, "work", "w1:p2", "term_2", &launch(&argv, Some("/tmp"), &empty, &h)).await.unwrap();
    assert!(started.accepted);
    assert_eq!(started.prestart.shell_pid, Some(10));
    let sent = &t.requests.lock().unwrap()[3].1;
    assert_eq!(sent["keys"], json!(["Enter"]));
    assert!(sent["text"].as_str().unwrap().starts_with("cd -- '/tmp' && "));
}

#[tokio::test]
async fn create_splits_unfocused_and_keeps_the_pane_when_tagging_is_lost() {
    let empty = BTreeMap::new();
    let h = hash();
    let split = Split {
        target_pane_id: "w1:p2",
        terminal_id: "term_2",
        direction: SplitDirection::Down,
        cwd: Some("/work"),
        env: &empty,
        launch_id: "launch-1",
        configuration_hash: &h,
    };
    let created = json!({"type":"pane_info","pane":{"pane_id":"w1:p3","terminal_id":"term_3",
        "workspace_id":"w1","tab_id":"w1:t1","cwd":"/work"}});
    let t = Scripted::new(vec![Some(Ok(panes())), Some(Ok(created)), None]);
    let result = create(&t, "work", &split).await.unwrap();
    assert_eq!(result.pane.pane_id, "w1:p3");
    assert_eq!(result.correlation, Correlation::Unknown);
    let requests = t.requests.lock().unwrap().clone();
    assert_eq!(requests[1].1["focus"], false);
    assert_eq!(requests[1].1["direction"], "down");
    assert_eq!(requests[1].1["env"]["HYDRA_LAUNCH_ID"], "launch-1");
    assert_eq!(requests[2].1["tokens"]["hydra_configuration_hash"], h);
    // A lost split reply is an unknown outcome; nothing else is attempted.
    let t = Scripted::new(vec![Some(Ok(panes())), None]);
    assert_eq!(create(&t, "work", &split).await.unwrap_err().kind, "classic_outcome_unknown");
    assert_eq!(t.methods(), ["pane.list", "pane.split"]);
}
