//! Helpers shared by the end-to-end tests: a fake Ollama, starting the real
//! `localrouterd` binary in a temporary folder, and a minimal HTTP client.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&self.body)))
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Minimal HTTP/1.1 client: one request per connection.
pub async fn exchange<S: AsyncReadExt + AsyncWriteExt + Unpin>(mut s: S, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
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

pub fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(nl) = b.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(std::str::from_utf8(&b[..nl]).unwrap().trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.extend_from_slice(&b[nl + 2..nl + 2 + size]);
        b = &b[nl + 2 + size + 2..];
    }
    out
}

pub struct Daemon {
    pub child: Child,
    pub dir: tempfile::TempDir,
    pub port: u16,
    pub seen: Arc<Mutex<Vec<Value>>>,
}

impl Daemon {
    pub fn socket(&self) -> PathBuf {
        self.dir.path().join("lr.sock")
    }
    pub async fn sock(&self, method: &str, path: &str, body: Option<Value>) -> Resp {
        let s = UnixStream::connect(self.socket()).await.unwrap();
        let bytes = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        let ct = [("Content-Type", "application/json")];
        exchange(s, method, path, if bytes.is_empty() { &[] } else { &ct }, &bytes).await
    }
    pub async fn tcp(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
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

pub async fn fake_ollama(seen: Arc<Mutex<Vec<Value>>>) -> u16 {
    let loaded: Arc<Mutex<Vec<(String, u64)>>> = Arc::default();
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
        .route("/api/ps", get({
            let loaded = loaded.clone();
            move || {
                let list: Vec<Value> = loaded.lock().unwrap().iter().map(|(m, c)| json!({ "name": m, "size": 1, "context_length": c })).collect();
                async move { Json(json!({ "models": list })) }
            }
        }))
        .route("/api/generate", post({
            let (seen, loaded) = (seen.clone(), loaded.clone());
            move |Json(v): Json<Value>| {
                seen.lock().unwrap().push(v.clone());
                let mut l = loaded.lock().unwrap();
                let model = v["model"].as_str().unwrap_or_default().to_string();
                l.retain(|(m, _)| *m != model);
                if v["keep_alive"] != json!(0) {
                    l.push((model, v["options"]["num_ctx"].as_u64().unwrap_or(4096)));
                }
                async move { Json(json!({ "model": v["model"], "response": "", "done": true })) }
            }
        }))
        .route("/api/pull", post(|Json(v): Json<Value>| async move {
            let lines: Vec<Value> = if v["model"] == "new:3b" {
                vec![
                    json!({ "status": "pulling manifest" }),
                    json!({ "status": "pulling aa", "digest": "sha256:aa", "total": 1000, "completed": 0 }),
                    json!({ "status": "pulling aa", "digest": "sha256:aa", "total": 1000, "completed": 1000 }),
                    json!({ "status": "pulling bb", "digest": "sha256:bb", "total": 24, "completed": 24 }),
                    json!({ "status": "verifying sha256 digest" }),
                    json!({ "status": "success" }),
                ]
            } else {
                vec![json!({ "status": "pulling manifest" }), json!({ "error": "pull model manifest: file does not exist" })]
            };
            lines.iter().map(|l| format!("{l}\n")).collect::<String>()
        }))
        .route("/api/chat", post(echo(seen.clone())))
        .route("/v1/chat/completions", post(echo(seen.clone())));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await });
    port
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

pub async fn start(prepare: impl FnOnce(&Path)) -> Daemon {
    start_with(prepare, false).await
}

/// With the computer's real Hugging Face cache, for tests that need speech models.
pub async fn start_real_cache() -> Daemon {
    start_with(|_| {}, true).await
}

async fn start_with(prepare: impl FnOnce(&Path), real_cache: bool) -> Daemon {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let ollama = fake_ollama(seen.clone()).await;
    let dir = tempfile::Builder::new().prefix("lrd").tempdir_in("/tmp").unwrap(); // short path: sockets max 104 bytes
    prepare(dir.path());
    let port = free_port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_localrouterd"));
    cmd.env("LR_SUPPORT_DIR", dir.path())
        .env("LR_PORT", port.to_string())
        .env("LR_OLLAMA_URL", format!("http://127.0.0.1:{ollama}"))
        // Never the real Askantis models folder.
        .env("ASKANTIS_MODELS", dir.path().join("askantis"));
    if !real_cache {
        cmd.env("HF_HUB_CACHE", dir.path().join("hub")); // an empty cache: no speech models
    }
    let child = cmd.spawn().unwrap();
    let endpoint = dir.path().join("endpoint.json");
    let t0 = Instant::now();
    while !endpoint.exists() {
        assert!(t0.elapsed() < Duration::from_secs(20), "localrouterd didn't start");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Daemon { child, dir, port, seen }
}

pub fn scribely_manifest() -> Value {
    json!({
        "app_id": "app.askantis.scribely", "name": "Scribely\u{202E}gnp.exe", "version": "2.0.0", "publisher": "Askantis",
        "reason": "Transcribe recordings",
        "roles": [{ "role": "chapters", "capability": "chat", "min_context": 32768, "languages": ["de"],
                    "suggestions": [{ "store": "ollama", "id": "qwen3.5:9b" }], "required": false, "label": "Chapters" }]
    })
}

