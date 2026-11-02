# localrouterd

Local Router's background service. It speaks the [Local Router Protocol](../../spec/PROTOCOL.md) on a Unix socket (apps on this Mac, identified by their code signature) and on `127.0.0.1`/`::1` (tools with an `lra_` key), and writes `endpoint.json` when it's ready.

```bash
cargo run -p localrouterd                      # ~/Library/Application Support/Local Router
cargo test -p localrouterd --features test-admin   # end-to-end tests against a fake Ollama
```

`LR_SUPPORT_DIR`, `LR_PORT` and `LR_OLLAMA_URL` override the state folder, the first TCP port and Ollama's address, for development and tests. The `test-admin` feature lets any socket caller use the admin endpoints; it exists only for the integration tests and must never be in a release build.

## What this version implements

| Protocol | Status |
|---|---|
| Transport, web-page protection, discovery file, shutdown cleanup (3, 4) | ✅ |
| Identity by code signature, trust levels, interpreters refused, approvals by identity, one prompt per identity (5) | ✅ |
| `lra_` keys for TCP clients (5.1) | ✅ |
| Roles: declaring, resolution order, global and per-app bindings (6) | ✅ |
| Inventory across Ollama, the Hugging Face cache and app-held models, filtered per app (7.1) | ✅ |
| Models apps run themselves (9) | ✅ |
| Events with resume, filtered per app (10) | ✅ (inventory and download events still to come) |
| Ollama pass-through: allowlist, role resolution, `num_ctx`, keep-alive limit (11.3) | ✅ |
| OpenAI chat and embeddings (11.2) | Passed through to Ollama's OpenAI layer with roles resolved; translation to Ollama's own API (for context control) still to come |
| Speech to text: `json`, `text`, `srt`, `vtt`, `verbose_json`, `lr_speakers_json`, `lr_segments`, `lr_offset` (11.4) | ✅ (streaming and `lr_priority` still to come) |
| Speaker diarization: JSON and RTTM (11.5) | ✅ |
| Holds (`/lr/v1/models/load`, `unload`) (7.2) | Still to come |
| Downloads, including mirrors (8) | Still to come; the capability isn't announced until then |
