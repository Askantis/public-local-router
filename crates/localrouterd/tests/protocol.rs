//! End-to-end tests of `localrouterd` against a fake Ollama, over its real
//! Unix socket and TCP listener. The test binary is unsigned, so it goes
//! through the approval path; the `test-admin` feature lets it answer its own
//! prompt. Run: `cargo test -p localrouterd --features test-admin`.
#![cfg(feature = "test-admin")]

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[tokio::test]
async fn discovery_health_and_web_page_protection() {
    let d = start(|_| {}).await;
    let ep: Value = serde_json::from_slice(&std::fs::read(d.dir.path().join("endpoint.json")).unwrap()).unwrap();
    assert_eq!(ep["protocols"], json!([1]));
    assert_eq!(ep["http"], json!(format!("http://127.0.0.1:{}", d.port)));
    assert_eq!(ep["ollama"]["state"], "running");
    assert_eq!(ep["hf_cache"]["state"], "ok");
    assert!(std::fs::metadata(d.socket()).is_ok());

    let h = d.sock("GET", "/lr/v1/health", None).await;
    assert_eq!(h.status, 200);
    assert_eq!(h.json()["instance_id"], ep["instance_id"]);
    assert!(h.header("LR-Request-Id").unwrap().starts_with("req_"));
    assert_eq!(d.tcp("GET", "/lr/v1/health", &[], b"").await.status, 200);

    // DNS rebinding, cross-site requests and plain HTML forms are refused.
    assert_eq!(d.tcp("GET", "/lr/v1/health", &[("Host", "evil.example:11436")], b"").await.status, 403);
    assert_eq!(d.tcp("GET", "/lr/v1/health", &[("Origin", "https://evil.example")], b"").await.status, 403);
    let form = d.tcp("POST", "/api/chat", &[("Content-Type", "application/x-www-form-urlencoded")], b"model=x").await;
    assert_eq!(form.status, 415);
    // Without a key, TCP gets nothing but health.
    let me = d.tcp("GET", "/lr/v1/me", &[], b"").await;
    assert_eq!(me.status, 401);
    assert_eq!(me.json()["error"]["code"], "unauthorized");
    // Connecting is socket-only.
    let c = d.tcp("POST", "/lr/v1/connect", &[("Content-Type", "application/json")], scribely_manifest().to_string().as_bytes()).await;
    assert_eq!(c.status, 403);
    // The state folder is the user's only.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(d.dir.path()).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(d.socket()).unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
async fn approval_flow_roles_and_passthrough() {
    let d = start(|_| {}).await;
    // An unsigned caller (this test) has to be approved.
    let c = d.sock("POST", "/lr/v1/connect", Some(scribely_manifest())).await;
    assert_eq!(c.status, 202, "{}", String::from_utf8_lossy(&c.body));
    let rid = c.json()["request_id"].as_str().unwrap().to_string();
    assert!(rid.starts_with("cr_") && rid.len() >= 3 + 32, "{rid}");
    // At most one prompt per identity.
    assert_eq!(d.sock("POST", "/lr/v1/connect", Some(scribely_manifest())).await.json()["request_id"], rid);
    assert_eq!(d.sock("GET", &format!("/lr/v1/connect/{rid}"), None).await.json()["status"], "pending");
    assert_eq!(d.sock("GET", "/lr/v1/me", None).await.json()["error"]["code"], "not_approved");

    assert_eq!(d.sock("POST", &format!("/lr/v1/admin/connect/{rid}"), Some(json!({ "approve": true }))).await.status, 204);
    let s = d.sock("GET", &format!("/lr/v1/connect/{rid}"), None).await.json();
    assert_eq!(s["status"], "approved");
    assert_eq!(s["app"]["trust"], "unsigned");
    assert_eq!(s["app"]["name"], "Scribelygnp.exe", "bidi override removed");

    // chapters → the suggestion, since nothing is bound; chat has no global binding yet.
    let me = d.sock("GET", "/lr/v1/me", None).await.json();
    let role = |me: &Value, r: &str| me["roles"].as_array().unwrap().iter().find(|b| b["role"] == r).cloned().unwrap();
    assert_eq!(role(&me, "chapters")["model"], "qwen3.5:9b");
    assert_eq!(role(&me, "chapters")["source"], "suggestion");
    assert_eq!(role(&me, "chat")["model"], Value::Null);

    // Ollama pass-through: role resolved, context filled in, keep-alive limited, lr_ fields removed.
    let r = d.sock("POST", "/api/chat", Some(json!({ "model": "localrouter/chapters", "messages": [], "keep_alive": -1, "lr_context": null }))).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.header("LR-Resolved-Model"), Some("qwen3.5:9b"));
    let sent = d.seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(sent["model"], "qwen3.5:9b");
    assert_eq!(sent["options"]["num_ctx"], 32768);
    assert_eq!(sent["keep_alive"], 14400);
    assert!(sent.get("lr_context").is_none());
    // Only running and reading endpoints pass; errors keep Ollama's shape.
    let pull = d.sock("POST", "/api/pull", Some(json!({ "model": "x" }))).await;
    assert_eq!(pull.status, 403);
    assert!(pull.json()["error"].is_string());
    // An unbound role gets role_unbound with a link.
    let unbound = d.sock("POST", "/v1/chat/completions", Some(json!({ "model": "localrouter/chat", "messages": [] }))).await;
    assert_eq!(unbound.status, 409);
    assert_eq!(unbound.json()["error"]["details"]["link"], "localrouter://apps?app=app.askantis.scribely&role=chat");

    // A global chat binding that doesn't fit chapters' context is skipped; one that fits wins.
    assert_eq!(d.sock("PUT", "/lr/v1/admin/roles/chat", Some(json!({ "model": "tiny:1b" }))).await.status, 204);
    let me = d.sock("GET", "/lr/v1/me", None).await.json();
    assert_eq!(role(&me, "chat")["model"], "tiny:1b");
    assert_eq!(role(&me, "chapters")["model"], "qwen3.5:9b");
    // The user's choice for this app wins, and a missing model is reported, not replaced.
    d.sock("PUT", "/lr/v1/admin/apps/app.askantis.scribely/roles/chapters", Some(json!({ "model": "deleted:7b" }))).await;
    let gone = d.sock("POST", "/api/chat", Some(json!({ "model": "localrouter/chapters", "messages": [] }))).await;
    assert_eq!(gone.status, 409);

    // OpenAI-style requests are passed on with the role resolved.
    d.sock("PUT", "/lr/v1/admin/apps/app.askantis.scribely/roles/chapters", Some(json!({ "model": "qwen3.5:9b" }))).await;
    let oa = d.sock("POST", "/v1/chat/completions", Some(json!({ "model": "localrouter/chapters", "messages": [], "lr_context": 1 }))).await;
    assert_eq!(oa.status, 200);
    assert_eq!(d.seen.lock().unwrap().last().unwrap()["model"], "qwen3.5:9b");
    let list = d.sock("GET", "/v1/models", None).await.json();
    let ids: Vec<&str> = list["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"localrouter/chapters") && ids.contains(&"qwen3.5:9b"), "{ids:?}");

    // Inventory shows models with context lengths and the caller's bindings only.
    let models = d.sock("GET", "/lr/v1/models", None).await.json();
    let q = models["models"].as_array().unwrap().iter().find(|m| m["id"] == "qwen3.5:9b").unwrap().clone();
    assert_eq!(q["context_length"], 262144);
    assert_eq!(q["bound_for_you"], json!(["chapters"]));
    let emb = d.sock("GET", "/lr/v1/models?kind=embedding", None).await.json();
    assert_eq!(emb["models"].as_array().unwrap().len(), 1);

    // Disconnecting removes the approval.
    assert_eq!(d.sock("DELETE", "/lr/v1/me", None).await.status, 204);
    assert_eq!(d.sock("GET", "/lr/v1/me", None).await.status, 403);
}

#[tokio::test]
async fn keys_for_tcp_clients() {
    let d = start(|_| {}).await;
    let k = d.sock("POST", "/lr/v1/admin/keys", Some(json!({ "name": "Terminal tool" }))).await;
    assert_eq!(k.status, 201);
    let secret = k.json()["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("lra_") && secret.len() == 47);
    let stored = std::fs::read_to_string(d.dir.path().join("apps.json")).unwrap();
    assert!(!stored.contains(&secret), "only the hash is stored");
    let auth = format!("Bearer {secret}");
    let me = d.tcp("GET", "/lr/v1/me", &[("Authorization", &auth)], b"").await;
    assert_eq!(me.status, 200);
    assert_eq!(me.json()["trust"], "key");
    let wrong = d.tcp("GET", "/lr/v1/me", &[("Authorization", "Bearer lra_wrong")], b"").await;
    assert_eq!(wrong.status, 401);
    let body = json!({ "model": "qwen3.5:9b", "messages": [] }).to_string();
    let chat = d.tcp("POST", "/api/chat", &[("Authorization", &auth), ("Content-Type", "application/json")], body.as_bytes()).await;
    assert_eq!(chat.status, 200);
    // Revoked keys stop working.
    let id = k.json()["id"].as_str().unwrap().to_string();
    assert_eq!(d.sock("DELETE", &format!("/lr/v1/admin/keys/{id}"), None).await.status, 204);
    assert_eq!(d.tcp("GET", "/lr/v1/me", &[("Authorization", &auth)], b"").await.status, 401);
}

#[tokio::test]
async fn an_unsigned_program_cant_take_a_signed_apps_id() {
    let d = start(|dir| {
        let store = json!({ "apps": { "team:ABCDE12345/id:com.example.signed": {
            "app_id": "com.example.signed", "name": "Signed", "version": "1", "publisher": null, "trust": "signed",
            "connected_at": "2026-11-02T00:00:00Z", "last_used": null, "roles": [], "bindings": {} } } });
        std::fs::write(dir.join("apps.json"), store.to_string()).unwrap();
    })
    .await;
    let mut m = scribely_manifest();
    m["app_id"] = json!("com.example.signed");
    let c = d.sock("POST", "/lr/v1/connect", Some(m)).await;
    assert_eq!(c.status, 403);
    assert_eq!(c.json()["error"]["code"], "identity_mismatch");
    let mut bad = scribely_manifest();
    bad["app_id"] = json!("not-reverse-dns");
    assert_eq!(d.sock("POST", "/lr/v1/connect", Some(bad)).await.status, 400);
}

#[tokio::test]
async fn events_reach_the_right_app_and_shutdown_cleans_up() {
    let d = start(|_| {}).await;
    let rid = d.sock("POST", "/lr/v1/connect", Some(scribely_manifest())).await.json()["request_id"].as_str().unwrap().to_string();
    d.sock("POST", &format!("/lr/v1/admin/connect/{rid}"), Some(json!({ "approve": true }))).await;

    // Open the event stream, then bind a role: the app gets role.bound.
    let mut s = UnixStream::connect(d.socket()).await.unwrap();
    s.write_all(b"GET /lr/v1/events HTTP/1.1\r\nHost: lr\r\n\r\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    d.sock("PUT", "/lr/v1/admin/apps/app.askantis.scribely/roles/chapters", Some(json!({ "model": "qwen3.5:9b" }))).await;
    let mut got = String::new();
    let mut buf = [0u8; 4096];
    let t0 = Instant::now();
    while !got.contains("event: role.bound") {
        assert!(t0.elapsed() < Duration::from_secs(5), "no role.bound event in: {got}");
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await.unwrap().unwrap();
        got.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(got.contains("\"model\":\"qwen3.5:9b\""));
    assert!(!got.contains("connect.updated"), "apps don't see other apps' connect requests");

    assert_eq!(d.sock("POST", "/lr/v1/admin/shutdown", None).await.status, 202);
    let t0 = Instant::now();
    while d.dir.path().join("endpoint.json").exists() {
        assert!(t0.elapsed() < Duration::from_secs(5), "discovery file not removed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!d.socket().exists(), "socket removed");
}
