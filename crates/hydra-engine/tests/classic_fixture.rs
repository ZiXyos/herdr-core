//! Runs only against the isolated loopback fixture (hydra-native scripts/classic-ssh-fixture.sh).
//! Ignored by default so the normal suite never dials anything. Run explicitly with
//! `cargo test -- --ignored` and the HYDRA_FIXTURE_* variables; it FAILS when they are missing.

use std::time::Duration;

use hydra_engine::Engine;
use serde_json::{Value, json};

struct Fixture {
    port: u16,
    key: String,
    fingerprint: String,
}

fn fixture() -> Fixture {
    let var = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| {
            panic!("{name} is unset: start scripts/classic-ssh-fixture.sh and export its `env` output")
        })
    };
    let port = var("HYDRA_FIXTURE_PORT").parse().expect("HYDRA_FIXTURE_PORT is a port number");
    let key = std::fs::read_to_string(var("HYDRA_FIXTURE_KEY")).expect("HYDRA_FIXTURE_KEY is readable");
    Fixture { port, key, fingerprint: var("HYDRA_FIXTURE_FINGERPRINT") }
}

async fn next_event(engine: &Engine, within: Duration) -> Value {
    tokio::time::timeout(within, engine.next_event()).await.expect("event in time").expect("engine running")
}

/// Drains queued events without waiting.
async fn drain(engine: &Engine) -> Vec<Value> {
    let mut events = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(100), engine.next_event()).await {
        events.push(event);
    }
    events
}

fn connect_params(f: &Fixture, expected: Option<&str>) -> Value {
    json!({"host":"127.0.0.1","port":f.port,"username":"fixture","session":"compat","privateKey":f.key,"expectedFingerprint":expected})
}

async fn call(engine: &std::sync::Arc<Engine>, id: &str, method: &str, params: Value) -> Result<Value, Value> {
    engine.execute(id.into(), method.into(), params).await.map_err(|e| serde_json::to_value(e).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the loopback classic fixture (HYDRA_FIXTURE_PORT/KEY/FINGERPRINT)"]
async fn embedded_ssh_controls_existing_classic_pane() {
    let f = fixture();
    let engine = Engine::new();

    let unknown = call(&engine, "1", "connect", connect_params(&f, None)).await.unwrap_err();
    assert_eq!(unknown["kind"], "host_key_unverified");
    assert_eq!(unknown["presentedKey"]["fingerprint"], f.fingerprint);
    assert!(!unknown.to_string().contains("PRIVATE KEY"), "secrets must not appear in errors");

    let wrong = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let changed = call(&engine, "2", "connect", connect_params(&f, Some(wrong))).await.unwrap_err();
    assert_eq!(changed["kind"], "host_key_changed");
    assert_eq!(changed["expectedFingerprint"], wrong);

    let session = call(&engine, "3", "connect", connect_params(&f, Some(&f.fingerprint))).await.unwrap();
    assert_eq!(session["version"], "0.9.5");
    assert_eq!(session["capabilities"], json!(hydra_engine::CAPABILITIES));
    let id = session["sessionId"].as_str().unwrap().to_string();

    let list = call(&engine, "4", "list", json!({"sessionId":id})).await.unwrap();
    let pane = list["panes"][0].clone();
    let (pane_id, terminal_id) = (pane["pane_id"].as_str().unwrap(), pane["terminal_id"].as_str().unwrap());
    assert!(pane.get("agent_status").is_none());

    let stale = call(
        &engine,
        "5",
        "send",
        json!({"sessionId":id,"paneId":pane_id,"terminalId":"term_reused","text":"echo NOPE","keys":["enter"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(stale["kind"], "classic_stale_identity");
    let events = drain(&engine).await;
    assert!(
        events.iter().all(|event| event["kind"] != "transportLost"),
        "a definite classic error is not a transport loss: {events:?}"
    );

    let sent = call(
        &engine,
        "6",
        "send",
        json!({"sessionId":id,"paneId":pane_id,"terminalId":terminal_id,"text":"echo HYDRA_NATIVE_$((40+2))","keys":["enter"]}),
    )
    .await
    .unwrap();
    assert_eq!(sent["accepted"], true);
    let mut seen = false;
    for attempt in 0..20 {
        let read = call(
            &engine,
            &format!("read-{attempt}"),
            "read",
            json!({"sessionId":id,"paneId":pane_id,"terminalId":terminal_id,"lines":50}),
        )
        .await
        .unwrap();
        if read["read"]["text"].as_str().unwrap().contains("HYDRA_NATIVE_42") {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(seen, "shell output observed through classic read");

    let reconnected = call(&engine, "7", "reconnect", json!({"sessionId":id})).await.unwrap();
    assert_eq!(reconnected["sessionId"], id.as_str());
    let again = call(&engine, "8", "list", json!({"sessionId":id})).await.unwrap();
    assert_eq!(again["panes"][0]["terminal_id"], terminal_id, "reconnect keeps the same native runtime");
    assert_eq!(again["panes"].as_array().unwrap().len(), list["panes"].as_array().unwrap().len(), "no duplicate panes");

    let cancelled = {
        let engine2 = engine.clone();
        let id2 = id.clone();
        let task = tokio::spawn(async move { call(&engine2, "9", "list", json!({"sessionId":id2})).await });
        tokio::task::yield_now().await;
        engine.cancel("9");
        task.await.unwrap()
    };
    // Either the cancel won or the list finished first; both are explicit, never a hang.
    if let Err(error) = cancelled {
        assert_eq!(error["kind"], "cancelled");
    }

    // Only the stable bridge can be opened; raw classic API access is not exposed.
    let rejected = call(&engine, "ch-0", "channelOpen", json!({"sessionId":id,"kind":"control"})).await.unwrap_err();
    assert_eq!(rejected["kind"], "invalid_params");
    let rejected = call(&engine, "ch-5", "channelOpen", json!({"sessionId":id,"kind":"shell"})).await.unwrap_err();
    assert_eq!(rejected["kind"], "invalid_params");
    let opened = call(&engine, "ch-1", "channelOpen", json!({"sessionId":id,"kind":"stable"})).await.unwrap();
    let channel_id = opened["channelId"].as_str().unwrap().to_string();
    use base64::Engine as _;
    let too_big = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 65537]);
    let error =
        call(&engine, "ch-2", "channelWrite", json!({"channelId":channel_id,"data":too_big})).await.unwrap_err();
    assert_eq!(error["kind"], "invalid_params");
    call(&engine, "ch-3", "channelClose", json!({"channelId":channel_id})).await.unwrap();
    let closed = loop {
        let event = next_event(&engine, Duration::from_secs(5)).await;
        if event["kind"] == "channelClosed" {
            break event;
        }
    };
    assert_eq!(closed, json!({"kind":"channelClosed","channelId":channel_id,"reason":"closed"}));
    let closed = call(&engine, "ch-4", "channelWrite", json!({"channelId":channel_id,"data":""})).await.unwrap_err();
    assert_eq!(closed["kind"], "channel_not_found");
    // Disconnect closes the session's channels too.
    let opened = call(&engine, "ch-6", "channelOpen", json!({"sessionId":id,"kind":"stable"})).await.unwrap();
    let orphan = opened["channelId"].as_str().unwrap().to_string();

    call(&engine, "10", "disconnect", json!({"sessionId":id})).await.unwrap();
    let gone = call(&engine, "11", "list", json!({"sessionId":id})).await.unwrap_err();
    assert_eq!(gone["kind"], "session_not_found");
    let orphan_write = call(&engine, "12", "channelWrite", json!({"channelId":orphan,"data":""})).await.unwrap_err();
    assert_eq!(orphan_write["kind"], "channel_not_found");
    engine.shutdown().await;
}

/// Two verified sessions plus a stalled third connect run concurrently on one engine; cancelling the
/// stalled one does not disturb the others.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the loopback classic fixture (HYDRA_FIXTURE_PORT/KEY/FINGERPRINT)"]
async fn sessions_are_concurrent_and_independent() {
    let f = fixture();
    let engine = Engine::new();
    let stalled = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut stalled_params = connect_params(&f, Some(&f.fingerprint));
    stalled_params["port"] = json!(stalled.local_addr().unwrap().port());
    let hung = {
        let engine = engine.clone();
        tokio::spawn(async move { call(&engine, "hung", "connect", stalled_params).await })
    };

    let started = std::time::Instant::now();
    let (a, b) = tokio::join!(
        call(&engine, "a", "connect", connect_params(&f, Some(&f.fingerprint))),
        call(&engine, "b", "connect", connect_params(&f, Some(&f.fingerprint))),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a["sessionId"], b["sessionId"]);
    let (la, lb) = tokio::join!(
        call(&engine, "la", "list", json!({"sessionId":a["sessionId"]})),
        call(&engine, "lb", "list", json!({"sessionId":b["sessionId"]})),
    );
    assert_eq!(la.unwrap()["panes"], lb.unwrap()["panes"]);
    assert!(started.elapsed() < Duration::from_secs(8), "verified sessions did not wait for the stalled connect");
    assert!(!hung.is_finished(), "stalled connect still in flight");

    assert!(engine.cancel("hung"));
    assert_eq!(hung.await.unwrap().unwrap_err()["kind"], "cancelled");
    let events = drain(&engine).await;
    let connected: Vec<_> = events.iter().filter(|event| event["kind"] == "connected").collect();
    assert_eq!(connected.len(), 2, "{events:?}");
    assert!(events.iter().all(|event| event["kind"] != "transportLost"));
    engine.shutdown().await;
}

fn surface_text(surface: &Value) -> String {
    surface["cells"]
        .as_array()
        .map(|cells| cells.iter().filter_map(|c| c["symbol"].as_str()).collect())
        .unwrap_or_default()
}

/// Waits for a terminal event of `terminal_id` matching `pred`, skipping others.
async fn wait_terminal(engine: &Engine, terminal_id: &str, within: Duration, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = next_event(engine, left).await;
        if event["kind"] == "terminal" && event["terminalId"] == terminal_id && pred(&event) {
            return event;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the loopback classic fixture (HYDRA_FIXTURE_PORT/KEY/FINGERPRINT)"]
async fn launch_and_decoded_terminal_share_one_embedded_session() {
    let f = fixture();
    let engine = Engine::new();
    let session = call(&engine, "c", "connect", connect_params(&f, Some(&f.fingerprint))).await.unwrap();
    let id = session["sessionId"].as_str().unwrap().to_string();
    for capability in ["classic.create", "classic.start", "classic.inspect", "terminal.stable"] {
        assert!(session["capabilities"].as_array().unwrap().iter().any(|c| c == capability), "{capability}");
    }
    let list = call(&engine, "l", "list", json!({"sessionId":id})).await.unwrap();
    let pane = list["panes"][0].clone();
    let hash = "b".repeat(64);
    let launch_id = format!("int-{}", std::process::id());

    // Control characters are refused before any native request, on create and start alike.
    let bad = call(
        &engine,
        "bad",
        "start",
        json!({"sessionId":id,"paneId":pane["pane_id"],"terminalId":pane["terminal_id"],
        "argv":["/bin/echo","x\u{15}touch /tmp/PWN #"],"launchId":launch_id,"configurationHash":hash}),
    )
    .await
    .unwrap_err();
    assert_eq!(bad["kind"], "invalid_params");
    let bad = call(
        &engine,
        "bad2",
        "create",
        json!({"sessionId":id,"targetPaneId":pane["pane_id"],"terminalId":pane["terminal_id"],
        "direction":"down","cwd":"relative","launchId":launch_id,"configurationHash":hash}),
    )
    .await
    .unwrap_err();
    assert_eq!(bad["kind"], "invalid_params");

    let created = call(
        &engine,
        "cr",
        "create",
        json!({"sessionId":id,"targetPaneId":pane["pane_id"],"terminalId":pane["terminal_id"],
        "direction":"down","cwd":"/tmp","launchId":launch_id,"configurationHash":hash}),
    )
    .await
    .unwrap();
    assert_eq!(created["correlation"], "recorded");
    let new_pane = created["pane"].clone();
    let listed = call(&engine, "l2", "list", json!({"sessionId":id})).await.unwrap();
    let tagged =
        listed["panes"].as_array().unwrap().iter().find(|p| p["pane_id"] == new_pane["pane_id"]).unwrap().clone();
    assert_eq!(tagged["tokens"]["hydra_launch_id"], launch_id.as_str());

    let started = call(
        &engine,
        "st",
        "start",
        json!({"sessionId":id,"paneId":new_pane["pane_id"],"terminalId":new_pane["terminal_id"],
        "argv":["sleep","300"],"cwd":"/tmp","launchId":launch_id,"configurationHash":hash}),
    )
    .await
    .unwrap();
    assert_eq!(started["accepted"], true);
    let shell_pid = started["prestart"]["shell_pid"].clone();
    let mut observed = Value::Null;
    for attempt in 0..20 {
        let inspected = call(
            &engine,
            &format!("in{attempt}"),
            "inspect",
            json!({"sessionId":id,"paneId":new_pane["pane_id"],"terminalId":new_pane["terminal_id"]}),
        )
        .await
        .unwrap();
        let fg = inspected["process_info"]["foreground_processes"].clone();
        if fg.as_array().is_some_and(|p| p.iter().any(|p| p["argv"][0] == "sleep" && p["pid"] != shell_pid)) {
            observed = fg;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(!observed.is_null(), "the started process is observed as a new foreground process");
    // An occupied foreground is never typed into.
    let busy = call(
        &engine,
        "st2",
        "start",
        json!({"sessionId":id,"paneId":new_pane["pane_id"],"terminalId":new_pane["terminal_id"],
        "argv":["true"],"launchId":launch_id,"configurationHash":hash}),
    )
    .await
    .unwrap_err();
    assert_eq!(busy["kind"], "classic_shell_not_ready");
    drain(&engine).await;

    let opened = call(&engine, "to", "terminalOpen", json!({"sessionId":id,"terminalId":"t-int","cols":80,"rows":24}))
        .await
        .unwrap();
    let terminal = opened["terminalId"].as_str().unwrap().to_string();
    assert_eq!(terminal, "t-int");
    let dup =
        call(&engine, "to2", "terminalOpen", json!({"sessionId":id,"terminalId":"t-int","cols":80,"rows":24})).await;
    assert_eq!(dup.unwrap_err()["kind"], "invalid_params");
    assert_eq!(opened["epoch"], 1);
    let snapshot = wait_terminal(&engine, &terminal, Duration::from_secs(15), |e| e["type"] == "snapshot").await;
    assert_eq!(snapshot["epoch"], 1);
    assert!(snapshot["snapshot"]["panes"].as_array().unwrap().iter().any(|p| p["paneId"] == pane["pane_id"]));
    wait_terminal(&engine, &terminal, Duration::from_secs(15), |e| e["type"] == "surface").await;
    let input = json!({"terminalId":terminal,"paneId":pane["pane_id"],"events":[
        {"type":"text","text":"echo TERM_OK_$((6*7))"},{"type":"key","code":"enter"}]});
    call(&engine, "ti", "terminalInput", input).await.unwrap();
    let seen = wait_terminal(&engine, &terminal, Duration::from_secs(15), |e| {
        e["type"] == "surface" && surface_text(&e["surface"]).contains("TERM_OK_42")
    })
    .await;
    assert_eq!(seen["epoch"], 1);
    let bad_pane = call(
        &engine,
        "tb",
        "terminalInput",
        json!({"terminalId":terminal,"paneId":"w9:p9","events":[{"type":"text","text":"x"}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(bad_pane["kind"], "classic_stale_state", "{bad_pane}");

    let reconnected = call(&engine, "tr", "terminalReconnect", json!({"terminalId":terminal})).await.unwrap();
    assert_eq!(reconnected["epoch"], 2);
    let surface =
        wait_terminal(&engine, &terminal, Duration::from_secs(15), |e| e["type"] == "surface" && e["epoch"] == 2).await;
    assert!(surface_text(&surface["surface"]).contains("TERM_OK_42"), "same runtime after reconnect");
    call(&engine, "tc", "terminalClose", json!({"terminalId":terminal})).await.unwrap();
    let closed = call(
        &engine,
        "tc2",
        "terminalInput",
        json!({"terminalId":terminal,"paneId":pane["pane_id"],"events":[{"type":"text","text":"x"}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(closed["kind"], "terminal_not_found");
    // Interrupt the launched process so the pane returns to its shell.
    let ctrl_c = call(
        &engine,
        "cc",
        "send",
        json!({"sessionId":id,"paneId":new_pane["pane_id"],"terminalId":new_pane["terminal_id"],"keys":["Ctrl+c"]}),
    )
    .await;
    let _ = ctrl_c;
    call(&engine, "d", "disconnect", json!({"sessionId":id})).await.unwrap();
}

/// Needs `scripts/classic-ssh-fixture.sh bridge` (a fake bridge registered for pane w1:p1).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the loopback classic fixture with its fake bridge (classic-ssh-fixture.sh bridge)"]
async fn bridge_is_discovered_authenticated_and_carries_ordered_frames() {
    let f = fixture();
    let engine = Engine::new();
    let info = call(&engine, "1", "connect", connect_params(&f, Some(&f.fingerprint))).await.unwrap();
    assert!(info["capabilities"].as_array().unwrap().contains(&json!("harness.bridge")));
    let session = info["sessionId"].as_str().unwrap();
    drain(&engine).await;

    let listed = call(&engine, "2", "bridgeList", json!({"sessionId":session})).await.unwrap();
    let bridge = listed["bridges"].as_array().unwrap().iter().find(|b| b["paneId"] == "w1:p1").expect("fake bridge");
    assert_eq!(bridge["instanceId"], "fake-instance");
    assert_eq!(bridge["target"]["conversationId"], "fake-conversation");
    assert!(!listed.to_string().contains("fixture-token"), "the token stays in the engine");
    assert!(!listed.to_string().contains("bridge.sock"));

    let stale = json!({"sessionId":session,"bridgeId":"b0","paneId":"w1:p1","instanceId":"older-instance"});
    assert_eq!(call(&engine, "3", "bridgeOpen", stale).await.unwrap_err()["kind"], "bridge_unavailable");

    let open = json!({"sessionId":session,"bridgeId":"b1","paneId":"w1:p1","instanceId":"fake-instance"});
    let opened = call(&engine, "4", "bridgeOpen", open.clone()).await.unwrap();
    assert_eq!(opened["bridge"]["actorId"], "fixture-host");
    assert_eq!(call(&engine, "5", "bridgeOpen", open).await.unwrap_err()["kind"], "invalid_params");

    for (index, id) in ["r1", "r2"].iter().enumerate() {
        let line = json!({"protocol":"hydra.bridge","version":1,"requestId":id,"method":"hydra.hello","params":{}})
            .to_string();
        let sent =
            call(&engine, &format!("s{index}"), "bridgeSend", json!({"bridgeId":"b1","line":line})).await.unwrap();
        assert_eq!(sent["accepted"], true);
    }
    let mut frames = Vec::new();
    while frames.len() < 4 {
        let event = next_event(&engine, Duration::from_secs(10)).await;
        assert_eq!(event["kind"], "bridge", "{event}");
        assert_eq!((event["bridgeId"].as_str(), event["type"].as_str()), (Some("b1"), Some("frame")));
        frames.push(serde_json::from_str::<Value>(event["frame"].as_str().unwrap()).unwrap());
    }
    assert_eq!(frames[0], json!({"requestId":"r1","ok":true,"result":{"echo":"hydra.hello"}}));
    assert_eq!(frames[1]["event"]["after"], "r1");
    assert_eq!(frames[2]["requestId"], "r2");
    assert_eq!(frames[3]["type"], "hydra.event");

    let multiline = call(&engine, "6", "bridgeSend", json!({"bridgeId":"b1","line":"{}\n{}"})).await.unwrap_err();
    assert_eq!(multiline["kind"], "invalid_params");

    call(&engine, "7", "bridgeClose", json!({"bridgeId":"b1"})).await.unwrap();
    let closed = next_event(&engine, Duration::from_secs(5)).await;
    assert_eq!((closed["type"].as_str(), closed["reason"].as_str()), (Some("closed"), Some("closed")));
    let gone = call(&engine, "8", "bridgeSend", json!({"bridgeId":"b1","line":"{}"})).await.unwrap_err();
    assert_eq!(gone["kind"], "bridge_not_found");
    engine.shutdown().await;
}
