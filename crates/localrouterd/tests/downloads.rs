//! Downloads through `localrouterd` (PROTOCOL.md 8): a mirror into the
//! Hugging Face cache, an Ollama pull, models that are already there, and
//! requests that are refused.
//! Run: `cargo test -p localrouterd --features test-admin`.
#![cfg(feature = "test-admin")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use common::*;
use serde_json::{Value, json};
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// Connect as Scribely (this unsigned test binary needs the user's approval)
/// and wait until the Hugging Face cache is ready.
async fn ready() -> Daemon {
    let d = start(|_| {}).await;
    let rid = d.sock("POST", "/lr/v1/connect", Some(scribely_manifest())).await.json()["request_id"].as_str().unwrap().to_string();
    assert_eq!(d.sock("POST", &format!("/lr/v1/admin/connect/{rid}"), Some(json!({ "approve": true }))).await.status, 204);
    let t0 = Instant::now();
    while d.sock("GET", "/lr/v1/health", None).await.json()["hf_cache"]["state"] != "ok" {
        assert!(t0.elapsed() < Duration::from_secs(10), "cache never ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    d
}

/// A server for mirror files that counts its requests.
async fn mirror_server(body: &'static [u8]) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route("/blob", get(move || {
        h.fetch_add(1, Ordering::SeqCst);
        async move { body }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/blob?sig=not-for-disk", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (url, hits)
}

async fn wait_finished(d: &Daemon, id: &str) -> Value {
    let t0 = Instant::now();
    loop {
        let r = d.sock("GET", &format!("/lr/v1/downloads/{id}"), None).await.json();
        if ["done", "failed", "cancelled"].contains(&r["state"].as_str().unwrap_or("")) {
            return r;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "download didn't finish: {r}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn mirror_request(repo: &str, url: &str, body: &[u8]) -> Value {
    let sha = hex::encode(sha2::Sha256::digest(body));
    json!({
        "model": { "store": "hf", "id": repo }, "source": "mirror",
        "mirror": { "revision": COMMIT, "files": [{ "path": "ggml-model-q8_0.bin", "url": url, "size_bytes": body.len(), "sha256": sha, "etag": sha }] }
    })
}

#[tokio::test]
async fn a_mirror_lands_in_the_standard_cache_once() {
    let d = ready().await;
    let (url, hits) = mirror_server(b"grueezi weights").await;
    let r = d.sock("POST", "/lr/v1/downloads", Some(mirror_request("askantis/test-model", &url, b"grueezi weights"))).await;
    assert_eq!(r.status, 202, "{}", String::from_utf8_lossy(&r.body));
    let rec = r.json();
    assert_eq!(rec["model"], format!("hf:askantis/test-model@{COMMIT}"));
    assert_eq!(rec["started_by_you"], true);
    let done = wait_finished(&d, rec["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["bytes_done"], 15);

    let repo = d.dir.path().join("hub/models--askantis--test-model");
    assert_eq!(std::fs::read(repo.join("snapshots").join(COMMIT).join("ggml-model-q8_0.bin")).unwrap(), b"grueezi weights");
    assert_eq!(std::fs::read_to_string(repo.join("refs/main")).unwrap(), COMMIT);

    // Asked again: nothing is fetched twice.
    let again = d.sock("POST", "/lr/v1/downloads", Some(mirror_request("askantis/test-model", &url, b"grueezi weights"))).await.json();
    assert_eq!(wait_finished(&d, again["id"].as_str().unwrap()).await["state"], "done");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // The app's own list has both; the signed URL is kept nowhere on disk.
    let list = d.sock("GET", "/lr/v1/downloads", None).await.json();
    assert_eq!(list["downloads"].as_array().unwrap().len(), 2);
    let all = walk(d.dir.path());
    assert!(all.iter().all(|(p, c)| !p.contains("not-for-disk") && !c.contains("not-for-disk")));
}

#[tokio::test]
async fn mirrors_are_checked_before_anything_happens() {
    let d = ready().await;
    let (url, hits) = mirror_server(b"x").await;
    // Only the caller's own organisation.
    let other = d.sock("POST", "/lr/v1/downloads", Some(mirror_request("someone/else", &url, b"x"))).await;
    assert_eq!(other.status, 403);
    assert_eq!(other.json()["error"]["code"], "forbidden");
    // Paths that leave the repository, and addresses that aren't public https.
    let mut bad_path = mirror_request("askantis/x", &url, b"x");
    bad_path["mirror"]["files"][0]["path"] = json!("../../escape.bin");
    assert_eq!(d.sock("POST", "/lr/v1/downloads", Some(bad_path)).await.json()["error"]["code"], "invalid_request");
    let inside = mirror_request("askantis/x", "http://192.168.1.10/blob", b"x");
    assert_eq!(d.sock("POST", "/lr/v1/downloads", Some(inside)).await.json()["error"]["code"], "forbidden");
    // A file that doesn't match its checksum fails and stays invisible.
    let mut wrong = mirror_request("askantis/x", &url, b"y");
    wrong["mirror"]["files"][0]["size_bytes"] = json!(1);
    let id = d.sock("POST", "/lr/v1/downloads", Some(wrong)).await.json()["id"].as_str().unwrap().to_string();
    let failed = wait_finished(&d, &id).await;
    assert_eq!(failed["state"], "failed");
    assert!(failed["error"]["message"].as_str().unwrap().contains("checksum"), "{failed}");
    assert!(!d.dir.path().join("hub/models--askantis--x/snapshots").join(COMMIT).join("ggml-model-q8_0.bin").exists());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(d.sock("POST", "/lr/v1/downloads", Some(json!({ "model": { "store": "hf", "id": "a/b" }, "source": "ftp" }))).await.status, 400);
    assert_eq!(d.sock("GET", "/lr/v1/downloads/dl_nope", None).await.status, 404);
}

#[tokio::test]
async fn ollama_pulls_report_progress() {
    let d = ready().await;
    let mut events = UnixStream::connect(d.socket()).await.unwrap();
    events.write_all(b"GET /lr/v1/events HTTP/1.1\r\nHost: lr\r\n\r\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let r = d.sock("POST", "/lr/v1/downloads", Some(json!({ "model": { "store": "ollama", "id": "new:3b" }, "source": "ollama" }))).await;
    assert_eq!(r.status, 202, "{}", String::from_utf8_lossy(&r.body));
    let done = wait_finished(&d, r.json()["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["bytes_total"], 1024);

    let mut got = String::new();
    let mut buf = [0u8; 8192];
    let t0 = Instant::now();
    while !got.contains("event: download.done") {
        assert!(t0.elapsed() < Duration::from_secs(5), "no download.done in: {got}");
        let n = tokio::time::timeout(Duration::from_secs(5), events.read(&mut buf)).await.unwrap().unwrap();
        got.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(got.contains("event: download.progress"), "{got}");

    // Already in Ollama: done at once.
    let have = d.sock("POST", "/lr/v1/downloads", Some(json!({ "model": { "store": "ollama", "id": "qwen3.5:9b" }, "source": "ollama" }))).await;
    assert_eq!(have.status, 200);
    assert_eq!(have.json()["state"], "done");
    // Ollama's own error comes back in the record.
    let id = d.sock("POST", "/lr/v1/downloads", Some(json!({ "model": { "store": "ollama", "id": "missing:7b" }, "source": "ollama" }))).await.json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let failed = wait_finished(&d, &id).await;
    assert_eq!(failed["state"], "failed");
    assert!(failed["error"]["message"].as_str().unwrap().contains("does not exist"), "{failed}");
}

/// Every file under `dir` with its contents, for "nothing secret on disk".
fn walk(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                let c = String::from_utf8_lossy(&std::fs::read(&p).unwrap_or_default()).into_owned();
                out.push((p.display().to_string(), c));
            }
        }
    }
    out
}
