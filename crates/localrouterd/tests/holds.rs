//! Holding models (PROTOCOL.md 7.2) through `localrouterd` and a fake Ollama:
//! a role is loaded with its context, shows as held, and goes back to
//! Ollama's idle time when the app lets go or disconnects.
//! Run: `cargo test -p localrouterd --features test-admin`.
#![cfg(feature = "test-admin")]

mod common;

use common::*;
use serde_json::{Value, json};

async fn connected() -> Daemon {
    let d = start(|_| {}).await;
    let rid = d.sock("POST", "/lr/v1/connect", Some(scribely_manifest())).await.json()["request_id"].as_str().unwrap().to_string();
    assert_eq!(d.sock("POST", &format!("/lr/v1/admin/connect/{rid}"), Some(json!({ "approve": true }))).await.status, 204);
    d
}

fn last_generate(d: &Daemon) -> Value {
    d.seen.lock().unwrap().iter().rev().find(|v| v.get("prompt").is_some()).cloned().expect("a load request reached Ollama")
}

fn qwen(models: &Value) -> Value {
    models["models"].as_array().unwrap().iter().find(|m| m["id"] == "qwen3.5:9b").cloned().unwrap()
}

#[tokio::test]
async fn a_held_role_stays_loaded_until_released() {
    let d = connected().await;
    let r = d.sock("POST", "/lr/v1/models/load", Some(json!({ "model": "localrouter/chapters", "keep_alive": "10m" }))).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let held = r.json();
    assert_eq!(held["model"], "qwen3.5:9b");
    assert_eq!(held["context"], 32768, "the role's minimum context");
    let sent = last_generate(&d);
    assert_eq!(sent["keep_alive"], 660, "a little longer than the hold");
    assert_eq!(sent["options"]["num_ctx"], 32768);

    let q = qwen(&d.sock("GET", "/lr/v1/models", None).await.json());
    assert_eq!(q["state"], "loaded");
    assert_eq!(q["loaded"]["held_by_you"], true);
    assert_eq!(q["loaded"]["holders"], 1);

    // Letting go: back to Ollama's own idle time, with the same context so it isn't reloaded.
    assert_eq!(d.sock("POST", "/lr/v1/models/unload", Some(json!({ "model": "localrouter/chapters" }))).await.status, 204);
    let sent = last_generate(&d);
    assert!(sent.get("keep_alive").is_none(), "{sent}");
    assert_eq!(sent["options"]["num_ctx"], 32768);
    assert_eq!(qwen(&d.sock("GET", "/lr/v1/models", None).await.json())["loaded"]["held_by_you"], false);

    // Disconnecting ends holds too.
    d.sock("POST", "/lr/v1/models/load", Some(json!({ "model": "qwen3.5:9b" }))).await;
    assert_eq!(last_generate(&d)["keep_alive"], 15 * 60 + 60, "default 15 minutes");
    let before = d.seen.lock().unwrap().len();
    assert_eq!(d.sock("DELETE", "/lr/v1/me", None).await.status, 204);
    assert!(d.seen.lock().unwrap().len() > before, "released on disconnect");
    assert!(last_generate(&d).get("keep_alive").is_none());
}

#[tokio::test]
async fn holds_are_checked() {
    let d = connected().await;
    let missing = d.sock("POST", "/lr/v1/models/load", Some(json!({ "model": "nope:1b" }))).await;
    assert_eq!(missing.status, 409);
    assert_eq!(missing.json()["error"]["code"], "model_not_downloaded");
    let bad = d.sock("POST", "/lr/v1/models/load", Some(json!({ "model": "qwen3.5:9b", "keep_alive": "forever" }))).await;
    assert_eq!(bad.status, 400);
    assert_eq!(bad.json()["error"]["param"], "keep_alive");
    // No speech models in this cache: a clear error, not a crash.
    let speech = d.sock("POST", "/lr/v1/models/load", Some(json!({ "model": "localrouter/transcribe" }))).await;
    assert_eq!(speech.status, 409, "{}", String::from_utf8_lossy(&speech.body));
}
