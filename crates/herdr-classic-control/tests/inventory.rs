use herdr_classic_control::{ControlError, ControlTransport, Request, list};
use serde_json::{Value, json};
use std::sync::Mutex;

struct Snapshots(Mutex<Vec<Value>>);

impl ControlTransport for Snapshots {
    async fn exchange(&self, session: &str, request: Request) -> Result<Value, ControlError> {
        assert_eq!(session, "native");
        assert_eq!(request.method, "session.snapshot", "discovery never mutates or opens a viewer");
        assert_eq!(request.params, json!({}));
        assert!(!request.mutation);
        Ok(self.0.lock().unwrap().remove(0))
    }
}

fn snapshot() -> Value {
    json!({"type":"session_snapshot", "snapshot":{
        "panes":[
            {"pane_id":"opaque:p/7", "terminal_id":"term_native", "workspace_id":"opaque:w/9",
             "tab_id":"opaque:t/3", "agent":"some-agent", "agent_status":"working",
             "tokens":{"hydra_launch_id":"launch-1"}},
            {"pane_id":"opaque:p/8", "terminal_id":"term_shell", "workspace_id":"opaque:w/9",
             "tab_id":"opaque:t/3", "agent":null, "agent_status":"unknown"}
        ],
        "workspaces":[{"workspace_id":"opaque:w/9", "label":"  Projet 日本  ", "number":2,
                       "active_tab_id":"opaque:t/3"}],
        "tabs":[{"workspace_id":"opaque:w/9", "tab_id":"opaque:t/3", "label":"API 🚀", "number":4,
                 "active_pane_id":"opaque:p/7"}],
        "active_workspace_id":"opaque:w/9"
    }})
}

#[tokio::test]
async fn discovery_is_one_atomic_snapshot_and_refreshes_labels_without_changing_identity() {
    let first = snapshot();
    let mut renamed = first.clone();
    renamed["snapshot"]["workspaces"][0]["label"] = json!("Renamed workspace");
    renamed["snapshot"]["tabs"][0]["label"] = json!("Renamed tab");
    let transport = Snapshots(Mutex::new(vec![first.clone(), renamed.clone()]));
    for expected in [first, renamed] {
        let actual = serde_json::to_value(list(&transport, "native").await.unwrap()).unwrap();
        assert_eq!(
            actual["workspaces"][0],
            json!({
                "workspace_id":"opaque:w/9", "label":expected["snapshot"]["workspaces"][0]["label"], "number":2
            })
        );
        assert_eq!(
            actual["tabs"][0],
            json!({
                "workspace_id":"opaque:w/9", "tab_id":"opaque:t/3",
                "label":expected["snapshot"]["tabs"][0]["label"], "number":4
            })
        );
        assert_eq!(actual["panes"][0], expected["snapshot"]["panes"][0]);
        assert_eq!(actual["panes"][1]["pane_id"], "opaque:p/8");
        assert!(actual["panes"][1].get("agent").is_none());
        assert!(actual["panes"][1].get("agent_status").is_none());
    }
    assert!(transport.0.lock().unwrap().is_empty(), "exactly one request per list");
}

#[tokio::test]
async fn missing_null_and_empty_labels_are_not_fabricated() {
    for fields in [json!({}), json!({"label":null,"number":null}), json!({"label":"","number":0})] {
        let mut reply = snapshot();
        reply["snapshot"]["workspaces"][0] = fields.clone();
        reply["snapshot"]["workspaces"][0]["workspace_id"] = json!("opaque:w/9");
        reply["snapshot"]["tabs"][0] = fields.clone();
        reply["snapshot"]["tabs"][0]["workspace_id"] = json!("opaque:w/9");
        reply["snapshot"]["tabs"][0]["tab_id"] = json!("opaque:t/3");
        let transport = Snapshots(Mutex::new(vec![reply]));
        let actual = serde_json::to_value(list(&transport, "native").await.unwrap()).unwrap();
        for kind in ["workspaces", "tabs"] {
            assert_eq!(actual[kind][0]["label"], fields["label"]);
            assert_eq!(actual[kind][0]["number"], fields["number"]);
        }
    }
}

#[tokio::test]
async fn malformed_snapshots_fail_without_partial_inventory_or_fallback_requests() {
    let mut invalid =
        vec![json!({}), json!({"type":"session_snapshot"}), json!({"type":"session_snapshot","snapshot":null})];
    let mut wrong_type = snapshot();
    wrong_type["type"] = json!("pane_list");
    invalid.push(wrong_type);
    for kind in ["panes", "workspaces", "tabs"] {
        let mut missing = snapshot();
        missing["snapshot"].as_object_mut().unwrap().remove(kind);
        invalid.push(missing);
        let mut malformed = snapshot();
        malformed["snapshot"][kind] = json!([{}]);
        invalid.push(malformed);
    }
    for (kind, field, value) in
        [("workspaces", "label", json!(42)), ("tabs", "label", json!([])), ("tabs", "number", json!(-1))]
    {
        let mut malformed = snapshot();
        malformed["snapshot"][kind][0][field] = value;
        invalid.push(malformed);
    }
    for reply in invalid {
        let transport = Snapshots(Mutex::new(vec![reply.clone()]));
        assert_eq!(list(&transport, "native").await.unwrap_err().kind, "classic_protocol", "{reply}");
        assert!(transport.0.lock().unwrap().is_empty());
    }
    let transport = Snapshots(Mutex::new(vec![json!({"type":"session_snapshot","snapshot":{
        "panes":[],"workspaces":[],"tabs":[]
    }})]));
    assert_eq!(
        serde_json::to_value(list(&transport, "native").await.unwrap()).unwrap(),
        json!({"panes":[],"workspaces":[],"tabs":[]})
    );
}
