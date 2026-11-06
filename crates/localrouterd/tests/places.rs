//! The `hf` store's two folders (PROTOCOL.md 2): the Askantis models folder
//! and the Hugging Face cache, both temporary folders here. Models are found
//! in both, listed once, never downloaded twice; new ones go to the target,
//! which Local Router's window can change; removing respects other apps.
//! Run: `cargo test -p localrouterd --features test-admin`.
#![cfg(feature = "test-admin")]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use common::*;
use serde_json::{Value, json};
use sha2::Digest;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A file of a repository in the Hugging Face layout, at `refs/main`.
fn put(root: &Path, repo: &str, file: &str, body: &[u8]) {
    let dir = root.join(format!("models--{}", repo.replace('/', "--")));
    std::fs::create_dir_all(dir.join("refs")).unwrap();
    std::fs::write(dir.join("refs/main"), COMMIT).unwrap();
    let snap = dir.join("snapshots").join(COMMIT);
    std::fs::create_dir_all(&snap).unwrap();
    std::fs::write(snap.join(file), body).unwrap();
}

/// Start with the target already chosen (as Local Router's window would).
async fn start_with_target(target: &str, seed: impl FnOnce(&Path)) -> Daemon {
    let target = target.to_string();
    let d = start(move |dir| {
        let settings = json!({ "settings": { "auto_connect_askantis": true, "download_target": target } });
        std::fs::write(dir.join("apps.json"), settings.to_string()).unwrap();
        seed(dir);
    })
    .await;
    wait_ok(&d).await;
    d
}

/// Until both folders have been checked.
async fn wait_ok(d: &Daemon) {
    let t0 = Instant::now();
    loop {
        let h = d.sock("GET", "/lr/v1/health", None).await.json();
        if h["hf_cache"]["state"] == "ok" && h["askantis_models"]["state"] == "ok" {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "folders not ready: {h}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn endpoint(d: &Daemon) -> Value {
    serde_json::from_slice(&std::fs::read(d.dir.path().join("endpoint.json")).unwrap()).unwrap()
}

async fn hf_models(d: &Daemon) -> Vec<Value> {
    d.sock("GET", "/lr/v1/models?store=hf", None).await.json()["models"].as_array().unwrap().clone()
}

#[tokio::test]
async fn inventory_lists_both_folders_once_preferring_the_target() {
    let d = start_with_target("askantis", |dir| {
        // Turbo q8 in both, turbo f16 only in the Hugging Face cache.
        put(&dir.join("askantis"), "ggerganov/whisper.cpp", "ggml-large-v3-turbo-q8_0.bin", b"in askantis");
        put(&dir.join("hub"), "ggerganov/whisper.cpp", "ggml-large-v3-turbo-q8_0.bin", b"in the hf cache");
        put(&dir.join("hub"), "ggerganov/whisper.cpp", "ggml-large-v3-turbo.bin", b"f16");
    })
    .await;
    let models = hf_models(&d).await;
    let place = |id: &str| -> Vec<String> {
        models.iter().filter(|m| m["id"] == id).map(|m| m["place"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(place("hf:ggerganov/whisper.cpp/ggml-large-v3-turbo-q8_0.bin"), vec!["askantis"], "listed once, from the target");
    assert_eq!(place("hf:ggerganov/whisper.cpp/ggml-large-v3-turbo.bin"), vec!["huggingface"]);
    assert_eq!(models.len(), 2);
}

#[tokio::test]
async fn discovery_file_and_health_name_both_folders_and_the_target() {
    let d = start_with_target("askantis", |_| {}).await;
    let askantis = d.dir.path().join("askantis").display().to_string();
    let t0 = Instant::now();
    let ep = loop {
        let ep = endpoint(&d);
        if ep["askantis_models"]["state"] == "ok" {
            break ep;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "{ep}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(ep["askantis_models"]["path"], askantis.as_str());
    assert_eq!(ep["hf_cache"]["path"], d.dir.path().join("hub").display().to_string().as_str());
    assert_eq!(ep["download_target"], "askantis");
    assert_eq!(ep["download_path"], askantis.as_str());
    let h = d.sock("GET", "/lr/v1/health", None).await.json();
    assert_eq!(h["download_target"], "askantis");
    assert_eq!(h["askantis_models"]["path"], askantis.as_str());
    // Checking doesn't create the folder; only a download into it does.
    assert!(!d.dir.path().join("askantis").exists());
}

#[tokio::test]
async fn without_a_choice_the_target_follows_the_rule_and_is_kept() {
    // HF_HUB_CACHE is set for the test daemon, so this computer "uses Hugging Face".
    let d = start(|_| {}).await;
    wait_ok(&d).await;
    assert_eq!(endpoint(&d)["download_target"], "huggingface");
    let saved: Value = serde_json::from_slice(&std::fs::read(d.dir.path().join("apps.json")).unwrap()).unwrap();
    assert_eq!(saved["settings"]["download_target"], "huggingface");
}

#[tokio::test]
async fn the_window_reads_and_changes_the_target() {
    let d = start_with_target("huggingface", |dir| {
        put(&dir.join("hub"), "org/one", "a.bin", b"12345");
        put(&dir.join("hub"), "org/two", "b.bin", b"123");
    })
    .await;
    let r = d.sock("GET", "/lr/v1/admin/download_target", None).await;
    assert_eq!(r.status, 200);
    let body = r.json();
    assert_eq!(body["target"], "huggingface");
    assert_eq!(body["has_huggingface"], true);
    let hf = body["folders"].as_array().unwrap().iter().find(|f| f["place"] == "huggingface").unwrap().clone();
    assert_eq!(hf["models"], 2);
    assert_eq!(hf["bytes"], 8 + 2 * COMMIT.len() as u64, "files and refs");

    let r = d.sock("PUT", "/lr/v1/admin/download_target", Some(json!({ "target": "askantis" }))).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["target"], "askantis");
    let t0 = Instant::now();
    while endpoint(&d)["download_target"] != "askantis" {
        assert!(t0.elapsed() < Duration::from_secs(10), "discovery file not updated");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let bad = d.sock("PUT", "/lr/v1/admin/download_target", Some(json!({ "target": "custom", "path": "relative/models" }))).await;
    assert_eq!(bad.status, 400);
    assert_eq!(d.sock("PUT", "/lr/v1/admin/download_target", Some(json!({ "target": "elsewhere" }))).await.status, 400);
    let custom = d.dir.path().join("mine");
    let r = d.sock("PUT", "/lr/v1/admin/download_target", Some(json!({ "target": "custom", "path": custom }))).await;
    assert_eq!(r.json()["path"], custom.display().to_string().as_str());

    // Over TCP nobody may (admin endpoints are socket-only, PROTOCOL.md 5.3).
    let tcp = d.tcp("GET", "/lr/v1/admin/download_target", &[], b"").await;
    assert_ne!(tcp.status, 200);
}

#[tokio::test]
async fn a_model_in_the_other_folder_is_not_downloaded_again() {
    let d = start_with_target("askantis", |dir| put(&dir.join("hub"), "org/repo", "f.bin", b"already here")).await;
    // No network in this test: a download that went online would fail.
    let r = d.sock("POST", "/lr/v1/downloads", Some(json!({ "model": { "store": "hf", "id": "org/repo", "file": "f.bin" }, "source": "hf" }))).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["state"], "done");
    assert!(!d.dir.path().join("askantis").exists(), "nothing was put in the target");
}

#[tokio::test]
async fn new_models_go_to_the_target_folder() {
    let d = start_with_target("askantis", |_| {}).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route("/blob", get(move || {
        h.fetch_add(1, Ordering::SeqCst);
        async { &b"weights"[..] }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/blob", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    let sha = hex::encode(sha2::Sha256::digest(b"weights"));
    let req = json!({
        "model": { "store": "hf", "id": "askantis/test-model" }, "source": "mirror",
        "mirror": { "revision": COMMIT, "files": [{ "path": "m.bin", "url": url, "size_bytes": 7, "sha256": sha, "etag": sha }] }
    });
    let r = d.sock("POST", "/lr/v1/downloads", Some(req)).await;
    assert_eq!(r.status, 202, "{}", String::from_utf8_lossy(&r.body));
    let id = r.json()["id"].as_str().unwrap().to_string();
    let t0 = Instant::now();
    loop {
        let rec = d.sock("GET", &format!("/lr/v1/downloads/{id}"), None).await.json();
        if rec["state"] == "done" {
            break;
        }
        assert!(rec["state"] != "failed" && t0.elapsed() < Duration::from_secs(10), "{rec}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let file = d.dir.path().join("askantis/models--askantis--test-model/snapshots").join(COMMIT).join("m.bin");
    assert_eq!(std::fs::read(file).unwrap(), b"weights");
    assert!(!d.dir.path().join("hub/models--askantis--test-model").exists());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn removing_keeps_what_another_app_uses() {
    let d = start_with_target("askantis", |dir| {
        put(&dir.join("hub"), "org/shared", "f.bin", b"x");
        put(&dir.join("askantis"), "org/mine", "f.bin", b"x");
        put(&dir.join("hub"), "org/mine", "g.bin", b"x");
        let apps = dir.join("askantis/.apps");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(apps.join("app.example.other.json"), json!({ "app_id": "app.example.other", "models": ["org/shared"] }).to_string()).unwrap();
    })
    .await;
    let r = d.sock("DELETE", "/lr/v1/admin/models/hf:org%2Fshared", None).await;
    assert_eq!(r.status, 409, "{}", String::from_utf8_lossy(&r.body));
    assert!(d.dir.path().join("hub/models--org--shared").is_dir());

    assert_eq!(d.sock("DELETE", "/lr/v1/admin/models/hf:org/mine", None).await.status, 204);
    assert!(!d.dir.path().join("askantis/models--org--mine").exists(), "from the Askantis folder");
    assert!(!d.dir.path().join("hub/models--org--mine").exists(), "and from the Hugging Face cache");

    assert_eq!(d.sock("DELETE", "/lr/v1/admin/models/hf:org/mine", None).await.status, 404);
    assert_eq!(d.sock("DELETE", "/lr/v1/admin/models/qwen3.5:9b", None).await.status, 400);
}
