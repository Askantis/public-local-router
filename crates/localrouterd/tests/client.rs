//! `localrouter-client` against the real `localrouterd`: discover, connect
//! (through the approval path, since the test binary is unsigned), roles,
//! chat through a role, speech errors, disconnect.
//! Run: `cargo test -p localrouterd --features test-admin`.
#![cfg(feature = "test-admin")]

mod common;

use common::*;
use localrouter_client::{Error, LocalAi, Presence, TranscribeOptions, discover_in};
use serde_json::json;

#[tokio::test]
async fn an_app_connects_and_uses_its_roles() {
    let d = start(|_| {}).await;
    let Presence::Running(client) = discover_in(d.dir.path()).await else { panic!("Local Router should be running") };
    let client = client.with_app("app.askantis.scribely", "2.0.0");
    assert_eq!(client.health().await.unwrap().protocols, vec![1]);

    // Not connected yet.
    assert_eq!(client.me().await.unwrap_err().code(), Some("not_approved"));

    // connect() waits for the user's answer; answer it from "Local Router's window".
    let manifest: localrouter_client::ConnectRequest = serde_json::from_value(scribely_manifest()).unwrap();
    let approver = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // The same identity asking again gets the same request: that's how we find it.
        let rid = d.sock("POST", "/lr/v1/connect", Some(scribely_manifest())).await.json()["request_id"].as_str().unwrap().to_string();
        assert_eq!(d.sock("POST", &format!("/lr/v1/admin/connect/{rid}"), Some(json!({ "approve": true }))).await.status, 204);
    };
    let waited = std::sync::atomic::AtomicBool::new(false);
    let on_wait = || waited.store(true, std::sync::atomic::Ordering::SeqCst);
    let (app, ()) = tokio::join!(client.connect(&manifest, Some(&on_wait)), approver);
    let app = app.unwrap();
    assert!(waited.load(std::sync::atomic::Ordering::SeqCst), "the app was told to say 'allow it in Local Router'");
    assert_eq!(app.app_id, "app.askantis.scribely");
    let chapters = app.roles.iter().find(|r| r.role == "chapters").unwrap();
    assert_eq!(chapters.model.as_deref(), Some("qwen3.5:9b"));

    // Chat through the role.
    let answer = client.chat(json!({ "model": "localrouter/chapters", "messages": [{ "role": "user", "content": "Hoi" }] })).await.unwrap();
    assert_eq!(answer["model"], "qwen3.5:9b");
    let sent = d.seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(sent["options"]["num_ctx"], 32768);
    assert_eq!(sent["stream"], false);

    // No speech models in this test's cache: a clear error code, not a crash.
    let err = client.transcribe(b"RIFF".to_vec(), "x.wav", &TranscribeOptions::default()).await.unwrap_err();
    assert_eq!(err.code(), Some("role_unbound"), "{err}");

    // Downloads and holds.
    let d = client.download(&json!({ "model": { "store": "ollama", "id": "new:3b" }, "source": "ollama" })).await.unwrap();
    let mut seen = 0;
    let d = client.wait_download(d, |_| seen += 1).await.unwrap();
    assert_eq!(d.state, "done");
    assert_eq!(d.fraction(), Some(1.0));
    assert!(seen >= 1);
    let failed = client.download(&json!({ "model": { "store": "ollama", "id": "missing:7b" }, "source": "ollama" })).await.unwrap();
    assert!(client.wait_download(failed, |_| {}).await.unwrap_err().to_string().contains("does not exist"));
    let held = client.hold("localrouter/chapters", None, Some("5m")).await.unwrap();
    assert_eq!(held["context"], 32768);
    client.release("localrouter/chapters").await.unwrap();

    let ai = LocalAi::Connected(client.clone());
    assert!(ai.is_connected());

    client.disconnect().await.unwrap();
    assert!(matches!(client.me().await, Err(Error::Api(ref e)) if e.code == "not_approved"));
}

/// The embedded engine and the service give the same transcript for the same
/// audio. Needs the speech models in the standard Hugging Face cache
/// (`lr fetch`) and `bench/out/words.wav` (`bench/synth.py bench/out`).
/// Run: `cargo test -p localrouterd --features test-admin --release -- --ignored`.
#[tokio::test]
#[ignore]
async fn embedded_and_connected_agree() {
    let wav = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/out/words.wav");
    let audio = std::fs::read(&wav).expect("run bench/synth.py bench/out first");
    let opts = TranscribeOptions { language: Some("de".into()), ..Default::default() };
    let engine = localrouter_client::embedded::Engine::new(localrouter_speech::catalog::WHISPER_TURBO_Q8).unwrap();
    assert!(engine.ready(false));
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    let embedded = LocalAi::Embedded(engine)
        .transcribe_with_progress(audio.clone(), "words.wav", &opts, move |e| sink.lock().unwrap().push(e))
        .await
        .unwrap();
    assert!(embedded.text.contains("Guten Morgen"), "{}", embedded.text);
    let events = events.lock().unwrap();
    use localrouter_client::TranscribeEvent as E;
    assert_eq!(events[0], E::Phase("decode"));
    assert!(events.contains(&E::Phase("transcribe")));
    let streamed: Vec<_> = events.iter().filter_map(|e| if let E::Segment(s) = e { Some(s.clone()) } else { None }).collect();
    assert_eq!(streamed, embedded.segments, "every segment arrives once, as final");
    assert!(events.iter().any(|e| matches!(e, E::Fraction(f) if *f >= 1.0)));
    assert!(embedded.segments.iter().all(|s| s.speaker.is_none()));
    assert!(embedded.segments.iter().flat_map(|s| &s.words).count() >= 30);
}

/// Streaming through the service with the real speech models, segment by
/// segment. Needs `lr fetch` and `bench/synth.py bench/out`.
/// Run: `cargo test -p localrouterd --features test-admin --release -- --ignored`.
#[tokio::test]
#[ignore]
async fn segments_stream_in_as_they_finish() {
    let d = start_real_cache().await;
    let Presence::Running(client) = discover_in(d.dir.path()).await else { panic!("Local Router should be running") };
    let manifest: localrouter_client::ConnectRequest = serde_json::from_value(scribely_manifest()).unwrap();
    let approve = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let rid = d.sock("POST", "/lr/v1/connect", Some(scribely_manifest())).await.json()["request_id"].as_str().unwrap().to_string();
        d.sock("POST", &format!("/lr/v1/admin/connect/{rid}"), Some(json!({ "approve": true }))).await;
    };
    let (app, ()) = tokio::join!(client.connect(&manifest, None), approve);
    app.unwrap();
    let t0 = std::time::Instant::now();
    while d.sock("GET", "/lr/v1/health", None).await.json()["hf_cache"]["state"] != "ok" {
        assert!(t0.elapsed() < std::time::Duration::from_secs(30), "cache never ready");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let wav = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/out/words.wav");
    let opts = TranscribeOptions {
        model: Some(localrouter_speech::catalog::whisper_model_id(&localrouter_speech::catalog::WHISPER_TURBO_Q8)),
        language: Some("de".into()),
        ..Default::default()
    };
    let mut streamed = vec![];
    let t = client.transcribe_streaming(std::fs::read(wav).unwrap(), "words.wav", &opts, |s| streamed.push(s)).await.unwrap();
    assert!(streamed.len() >= 2, "{streamed:?}");
    assert_eq!(streamed.len(), t.segments.len());
    assert_eq!(streamed[0].text, t.segments[0].text);
    assert!(t.text.contains("Guten Morgen"), "{}", t.text);
    assert!(t.segments.iter().flat_map(|s| &s.words).count() >= 30);
}
