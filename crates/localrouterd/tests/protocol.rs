//! End-to-end tests of `localrouterd` against a fake Ollama, over its real
//! Unix socket and TCP listener. The test binary is unsigned, so it goes
//! through the approval path; the `test-admin` feature lets it answer its own
//! prompt. Run: `cargo test -p localrouterd --features test-admin`.
#![cfg(feature = "test-admin")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&self.body)))
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Minimal HTTP/1.1 client: one request per connection.
async fn exchange<S: AsyncReadExt + AsyncWriteExt + Unpin>(mut s: S, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
    let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str("Host: 127.0.0.1\r\n");
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("response head");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.lines();
    let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(": ").map(|(k, v)| (k.to_string(), v.to_string()))).collect();
    let mut body = raw[split + 4..].to_vec();
    if headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked")) {
        body = dechunk(&body);
    }
    Resp { status, headers, body }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(nl) = b.windows(2).position(|w| w == b"\r\n") else { break };
        let size = usize::from_str_radix(std::str::from_utf8(&b[..nl]).unwrap().trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.extend_from_slice(&b[nl + 2..nl + 2 + size]);
        b = &b[nl + 2 + size + 2..];
    }
    out
}

struct Daemon {
    child: Child,
    dir: tempfile::TempDir,
    port: u16,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Daemon {
    fn socket(&self) -> PathBuf {
        self.dir.path().join("lr.sock")
    }
    async fn sock(&self, method: &str, path: &str, body: Option<Value>) -> Resp {
        let s = UnixStream::connect(self.socket()).await.unwrap();
        let bytes = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        let ct = [("Content-Type", "application/json")];
        exchange(s, method, path, if bytes.is_empty() { &[] } else { &ct }, &bytes).await
    }
    async fn tcp(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
        let s = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        exchange(s, method, path, headers, body).await
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn fake_ollama(seen: Arc<Mutex<Vec<Value>>>) -> u16 {
    let echo = |seen: Arc<Mutex<Vec<Value>>>| {
        move |Json(v): Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(v.clone());
                Json(json!({ "model": v["model"], "message": { "role": "assistant", "content": "ok" }, "done": true }))
            }
        }
    };
    let app = Router::new()
        .route("/api/version", get(|| async { Json(json!({ "version": "0.34.4" })) }))
        .route("/api/tags", get(|| async {
            Json(json!({ "models": [
                { "name": "qwen3.5:9b", "digest": "d1", "size": 6_600_000_000u64, "details": { "format": "gguf", "quantization_level": "Q4_K_M", "parameter_size": "9B" }, "capabilities": ["completion", "tools", "thinking"] },
                { "name": "tiny:1b", "digest": "d2", "size": 900_000_000u64, "details": { "format": "gguf" }, "capabilities": ["completion"] },
                { "name": "nomic-embed-text:latest", "digest": "d3", "size": 270_000_000u64, "details": { "format": "gguf" }, "capabilities": ["embedding"] }
            ]}))
        }))
        .route("/api/show", post(|Json(v): Json<Value>| async move {
            let ctx = match v["model"].as_str() { Some("qwen3.5:9b") => 262_144, Some("tiny:1b") => 4096, _ => 8192 };
            Json(json!({ "model_info": { "general.architecture": "x", "x.context_length": ctx } }))
        }))
        .route("/api/ps", get(|| async { Json(json!({ "models": [] })) }))
        .route("/api/chat", post(echo(seen.clone())))
        .route("/v1/chat/completions", post(echo(seen.clone())));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await });
    port
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn start(prepare: impl FnOnce(&Path)) -> Daemon {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let ollama = fake_ollama(seen.clone()).await;
    let dir = tempfile::Builder::new().prefix("lrd").tempdir_in("/tmp").unwrap(); // short path: sockets max 104 bytes
    prepare(dir.path());
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_localrouterd"))
        .env("LR_SUPPORT_DIR", dir.path())
        .env("LR_PORT", port.to_string())
        .env("LR_OLLAMA_URL", format!("http://127.0.0.1:{ollama}"))
        .env("HF_HUB_CACHE", dir.path().join("hub")) // an empty cache: no speech models
        .spawn()
        .unwrap();
    let endpoint = dir.path().join("endpoint.json");
    let t0 = Instant::now();
    while !endpoint.exists() {
        assert!(t0.elapsed() < Duration::from_secs(20), "localrouterd didn't start");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Daemon { child, dir, port, seen }
}

fn scribely_manifest() -> Value {
    json!({
        "app_id": "app.askantis.scribely", "name": "Scribely\u{202E}gnp.exe", "version": "2.0.0", "publisher": "Askantis",
        "reason": "Transcribe recordings",
        "roles": [{ "role": "chapters", "capability": "chat", "min_context": 32768, "languages": ["de"],
                    "suggestions": [{ "store": "ollama", "id": "qwen3.5:9b" }], "required": false, "label": "Chapters" }]
    })
}

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
