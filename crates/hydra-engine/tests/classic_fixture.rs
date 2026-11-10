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
    assert_eq!(session["capabilities"], json!(["classic.list", "classic.read", "classic.send", "channel.stable"]));
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
