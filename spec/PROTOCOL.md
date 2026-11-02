# Local Router Protocol, version 1

Status: **Draft 2**, 2 November 2026. Machine-readable companion: [`openapi.yaml`](openapi.yaml), [`schemas/endpoint.schema.json`](schemas/endpoint.schema.json).

Local Router keeps the AI models on a computer in one place and runs them for the apps on that computer. This document is the contract between Local Router's background service, `localrouterd`, and the apps that use it. It is written for two readers: people building an app that connects to Local Router, and people working on Local Router itself.

The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are used as in [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).

## Contents

1. [Overview](#1-overview)
2. [Concepts](#2-concepts)
3. [Transport](#3-transport)
4. [Discovery](#4-discovery)
5. [Identity and connecting](#5-identity-and-connecting)
6. [Roles](#6-roles)
7. [Models](#7-models)
8. [Downloads](#8-downloads)
9. [Models that apps run themselves](#9-models-that-apps-run-themselves)
10. [Events](#10-events)
11. [Inference](#11-inference)
12. [Errors](#12-errors)
13. [Versioning](#13-versioning)
14. [Privacy rules for implementations](#14-privacy-rules-for-implementations)
15. [Rules for client libraries](#15-rules-for-client-libraries)
16. [Security considerations](#16-security-considerations)
17. [Platform notes](#17-platform-notes)

## 1. Overview

```
   app (any language)
     │  Unix socket  (apps on this computer, identified by code signature)
     │  TCP 127.0.0.1:11436  (tools that can only speak HTTP, with an lra_ key)
     ▼
 localrouterd ──► Ollama on 127.0.0.1:11434   (language models, embeddings)
     │
     └──────────► Hugging Face cache          (speech models: Whisper, diarization, VAD)
```

`localrouterd` offers three groups of endpoints:

| Group | Paths | What it is |
|---|---|---|
| Management | `/lr/v1/*` | Specific to Local Router: connecting, roles, the model inventory, downloads, events, speaker diarization |
| OpenAI-compatible | `/v1/*` | Chat, embeddings, model list and speech-to-text, in the shape of the OpenAI API |
| Ollama-compatible | `/api/*` | The parts of Ollama's API that run models, passed through to Ollama |

An app that only knows how to talk to "an OpenAI-compatible provider" or "an Ollama server" can use Local Router with a base URL and a key. An app that uses the management API gets more: one-click connect, models chosen by the user in one place, shared downloads and live progress.

Local Router never replaces the tools underneath it. Ollama stays the place where language models live and run. The Hugging Face cache stays the place where Hugging Face models live. Whatever any tool downloads there, every other tool can use.

## 2. Concepts

**App.** A program that uses Local Router. Every app has an `app_id` in reverse-DNS form, for example `app.askantis.scribely`. For a signed app on macOS, the `app_id` is its bundle identifier, taken from the code signature (section 5).

**Connection.** The user's permission for an app to use Local Router. Local Router remembers it until the user removes it in Local Router or the app disconnects.

**Role.** A job an app needs a model for, such as `transcribe` or `chapters`. Apps declare roles. The user, or Local Router on its own, binds each role to a model. Requests can then name the role instead of a model: `"model": "localrouter/chapters"`. Section 6 has the details.

**Model.** One entry in Local Router's inventory. Models live in a **store**:

| Store | Where | Managed through |
|---|---|---|
| `ollama` | Ollama's models folder | Ollama's API |
| `hf` | The Hugging Face cache, in its standard layout | Local Router, compatible with `huggingface_hub` |
| `app` | Inside an app's own process | The app, which reports it (section 9) |

**Model ids.** Strings in one of these forms:

| Form | Example | Meaning |
|---|---|---|
| Ollama name | `qwen3.5:9b`, `hf.co/unsloth/Qwen3-4B-Instruct-2507-GGUF:Q4_K_M` | A model in Ollama's store |
| `hf:<org>/<repo>` | `hf:avencera/speakrs-models` | A whole Hugging Face repository in the cache |
| `hf:<org>/<repo>/<path>` | `hf:ggerganov/whisper.cpp/ggml-large-v3-turbo-q8_0.bin` | One file of a repository in the cache |
| `app:<app_id>/<name>` | `app:app.askantis.scribely/whisper` | A model an app runs itself |
| `localrouter/<role>` | `localrouter/transcribe` | Whatever model the caller's role is bound to |

An `hf:` id MAY end in `@<revision>` (a 40-character commit hash or a branch name). Without it, the revision is the one `refs/main` points to. The prefix `localrouter/` is reserved and is never an Ollama namespace.

**Standard places.** Local Router and every app that follows this protocol find the Hugging Face cache like this, and MUST NOT keep models anywhere else:

1. `HF_HUB_CACHE`, if set.
2. Otherwise `HF_HOME` + `/hub`, if `HF_HOME` is set.
3. Otherwise `XDG_CACHE_HOME` + `/huggingface/hub`, if `XDG_CACHE_HOME` is set.
4. Otherwise `~/.cache/huggingface/hub`.

On macOS, apps started from the Finder do not see variables from shell profiles. An implementation on macOS MUST also check `launchctl getenv` for these variables before using the default. When `localrouterd` runs, the path it resolved is in the discovery file (section 4), and apps SHOULD use that path so every program agrees.

## 3. Transport

### 3.1 Listeners

| Listener | Address | Who uses it |
|---|---|---|
| Unix socket | macOS: `~/Library/Application Support/Local Router/lr.sock` | Apps on this computer. Preferred. Required for connecting and for Local Router's own window. |
| TCP | `127.0.0.1:11436` and `[::1]:11436` | Tools that can only use HTTP over TCP, with an `lra_` key |

If port 11436 is taken, `localrouterd` uses the first free port from 11437 to 11445 and writes the port it chose into the discovery file. Clients MUST read the port from the discovery file and MUST NOT assume 11436.

`localrouterd` MUST NOT listen on any address other than loopback. Sharing models with other devices on the network is a separate Local Router feature with its own port (11435 by default) and is not part of this protocol.

The folder that holds the socket MUST be readable and writable by the user only (mode `0700`).

### 3.2 HTTP

- HTTP/1.1. Connections MAY be kept alive. Request bodies MAY use chunked transfer encoding.
- JSON bodies are UTF-8 and use `Content-Type: application/json`. Audio uploads use `multipart/form-data`.
- On the socket, the `Host` header is not checked. Clients SHOULD send `Host: lr`.
- Every response carries `LR-Request-Id`, a unique id for that request. It helps when reading Local Router's Log Stream.
- Clients SHOULD send `LR-Client: <app_id>/<app version> <library>/<library version>`, for example `LR-Client: app.askantis.scribely/2.0.0 localrouter-client/0.1.0`. It is informational only and never grants anything.

### 3.3 Protection against web pages

A web page in a browser can send requests to loopback addresses. To stop pages from using Local Router, the TCP listener MUST:

- answer `403 forbidden` if the `Host` header is not `127.0.0.1:<port>`, `[::1]:<port>` or `localhost:<port>`, which blocks DNS-rebinding attacks;
- answer `403 forbidden` to any request that has an `Origin` header, unless the user has explicitly allowed that origin in Local Router (none are allowed by default);
- answer `415 unsupported_media_type` to a JSON endpoint called with any other content type, so a plain HTML form can't reach it;
- never send `Access-Control-Allow-Origin: *`.

## 4. Discovery

### 4.1 The discovery file

When `localrouterd` is ready to answer requests, it writes the discovery file:

| Platform | Path |
|---|---|
| macOS | `~/Library/Application Support/Local Router/endpoint.json` |
| Windows | `%LOCALAPPDATA%\Askantis\Local Router\endpoint.json` |
| Linux | `$XDG_RUNTIME_DIR/localrouter/endpoint.json` |

It writes to a temporary file in the same folder and renames it into place, so readers never see half a file. It deletes the file when it shuts down cleanly. The schema is [`schemas/endpoint.schema.json`](schemas/endpoint.schema.json). Example:

```json
{
  "protocols": [1],
  "daemon_version": "2.0.0",
  "instance_id": "6f1d1c2e-8f0b-4b8e-9d55-0a3f0b6c3a10",
  "pid": 4242,
  "started_at": "2026-11-02T09:41:00Z",
  "http": "http://127.0.0.1:11436",
  "socket": "/Users/me/Library/Application Support/Local Router/lr.sock",
  "hf_cache": { "path": "/Users/me/.cache/huggingface/hub", "state": "ok" },
  "ollama": { "url": "http://127.0.0.1:11434", "state": "running", "version": "0.34.4" },
  "capabilities": ["connect", "roles", "resident", "downloads", "events",
                   "audio.transcriptions", "audio.diarizations", "chat", "embeddings",
                   "ollama.passthrough"]
}
```

`protocols` lists every protocol version the service speaks (section 13). `hf_cache.state` is `ok`, `missing_volume` (the cache is on a drive that isn't connected), `unwritable`, or `checking` while the service is still finding out. Checking can take a while: macOS asks the user before a background process reads a removable drive. The `hf_cache.state` event announces the result. `ollama.state` is `running`, `stopped` or `not_installed`.

The file can be stale after a crash. A client MUST confirm with `GET /lr/v1/health` that the service answers and that `instance_id` matches before relying on it.

### 4.2 Finding Local Router from an app

A client works out one of four states and acts on it:

| State | How to tell | What the app does |
|---|---|---|
| `running` | Discovery file present and `/lr/v1/health` answers with the same `instance_id` | Use it (connect if not connected yet) |
| `installed` | No working discovery file, but Local Router is installed. On macOS: Launch Services knows the bundle id `ch.askantis.localrouter` | Offer "Start Local Router". To start it, launch the app in the background (`open -g -b ch.askantis.localrouter`), which starts the service without opening a window, then wait up to 10 seconds for the discovery file |
| `not_installed` | Neither | Offer "Get Local Router" once, linking to `https://github.com/Askantis/public-local-router/releases/latest`, and carry on without it |
| `unreachable` | Discovery file present but no answer after a start attempt | Carry on without it and offer to try again |

An app MUST work fully without Local Router, using the same standard places (section 2). An app MUST NOT ask the user again and again to install Local Router: offering it in onboarding and showing it in settings is enough.

## 5. Identity and connecting

### 5.1 Who is calling

**On the Unix socket**, `localrouterd` learns from the operating system which program is on the other end. On macOS it reads the peer's audit token (`LOCAL_PEERTOKEN`) and checks the code signature of that running process, not of a file on disk. It sorts the caller into a trust level:

| Trust | Meaning |
|---|---|
| `askantis` | An application bundle with a valid Developer ID or App Store signature that satisfies `anchor apple generic and certificate leaf[subject.OU] = "PB9T4S67XB"` (signed by Askantis), built with the hardened runtime |
| `signed` | Any other application bundle with a valid Developer ID or App Store signature and the hardened runtime |
| `unsigned` | Everything else: no signature, an ad-hoc signature, Apple's own command-line tools (such as `curl`), a command-line tool, or an app whose signature allows code injection (the entitlements `get-task-allow`, `disable-library-validation` or `allow-dyld-environment-variables`, or no hardened runtime) |
| `key` | A TCP client that sent a valid `lra_` key |

A socket client needs no key or password: the operating system vouches for it.

**Interpreters** such as `python`, `node`, `ruby`, `osascript` or a shell can't connect over the socket. Approving an interpreter would approve every script it ever runs. A connect request from one is answered with `403 forbidden`, and the error tells the user to create a key instead. An app built from a script language and packaged as its own signed application bundle is not an interpreter in this sense.

**On TCP**, the other end can't be identified reliably. TCP clients MUST send a key: `Authorization: Bearer lra_<key>`. The user creates keys in Local Router (Settings, Apps, "Connect another app") and gives each one a name. Local Router shows a key once and keeps only its SHA-256 hash. A request without a valid key gets `401 unauthorized`, except `GET /lr/v1/health`. A key client's `app_id` is `key:<key id>`. It can declare roles (section 6) and use models like any connected app, but it can't use the endpoints that need the socket.

Helpers that belong to an app, such as an XPC service or a login item, count as the same app when they have the same team and their identifier starts with `<app_id>.`.

### 5.2 Connecting

An app connects once, over the socket, and sends its manifest:

```http
POST /lr/v1/connect
Content-Type: application/json

{
  "app_id": "app.askantis.scribely",
  "name": "Scribely",
  "version": "2.0.0",
  "publisher": "Askantis",
  "icon_png": "<base64 PNG, 128×128, at most 64 KB>",
  "reason": "Transcribe recordings and write chapters",
  "roles": [
    { "role": "transcribe", "capability": "transcription", "languages": ["gsw", "de", "en", "fr", "it"],
      "suggestions": [{ "store": "hf", "id": "ggerganov/whisper.cpp", "file": "ggml-large-v3-turbo-q8_0.bin" }] },
    { "role": "diarize", "capability": "diarization",
      "suggestions": [{ "store": "hf", "id": "avencera/speakrs-models" }] },
    { "role": "chapters", "capability": "chat", "min_context": 32768, "languages": ["de"], "required": false,
      "label": "Chapters" }
  ]
}
```

Local Router keeps approvals by **identity**, never by the `app_id` an app claims:

| Trust | Identity |
|---|---|
| `askantis`, `signed` | The app's designated requirement (team plus bundle identifier) |
| `unsigned` | `unsigned:<cdhash>` of the running program, so a changed program has to ask again |

For `askantis` and `signed` callers, `app_id` MUST equal the bundle identifier in the code signature; otherwise the answer is `403 identity_mismatch`. An `unsigned` caller can't use an `app_id` that a signed app has used, which also gets `403 identity_mismatch`; Local Router shows its `app_id` as *not verified*.

The answer depends on the caller:

| Caller | Answer |
|---|---|
| This identity is already connected | `200` with `"status": "approved"` and the app's record |
| Trust `askantis`, and "Askantis apps connect automatically" is on (the default) | `200`, approved right away. The click in the app was the user's consent. |
| Anyone else | `202` with `"status": "pending"` and a `request_id`. Local Router asks the user. |

The approval prompt MUST show what the signature says, not what the app says:
- for signed apps, the bundle's display name and icon, and the organisation in the signing certificate;
- for unsigned callers, the path of the program, with the manifest's `name` marked as *not verified*.

Text from the manifest is shown only after removing control characters and bidirectional overrides. There is at most one waiting prompt per identity.

While a request is pending, the app polls `GET /lr/v1/connect/{request_id}` about once a second, over the socket. Only the program that sent the request can read it; for anyone else the answer is `404`. Request ids carry at least 128 bits of randomness. The status becomes `approved`, `denied` or, after 5 minutes without an answer, `expired`.

`DELETE /lr/v1/me` disconnects. It removes the approval, the app's role bindings, its holds (section 7.2) and its resident models.

### 5.3 Local Router's own window

The menu-bar app is a client like any other, with extra rights. The `/lr/v1/admin/*` endpoints answer only on the Unix socket, and only to a caller whose signature has the identifier `ch.askantis.localrouter` and the Askantis team. Each connection is checked when it opens, so there is no admin token to steal. These endpoints are internal to Local Router and may change without notice. They are listed in `openapi.yaml` for completeness.

## 6. Roles

### 6.1 Declaring roles

Apps declare roles when connecting, and can replace the list later with `PUT /lr/v1/me/roles`. A role declaration has these fields:

| Field | Type | Notes |
|---|---|---|
| `role` | string | Lowercase letters, digits, `_`, `-`, `.`; at most 32 characters |
| `capability` | `chat`, `embedding`, `transcription`, `diarization` | What the model must be able to do |
| `min_context` | integer | For `chat`: the context length the app needs, in tokens |
| `languages` | list of BCP 47 codes | Hint for suggestions and for the user. `gsw` is Swiss German. |
| `suggestions` | list of model references | Models the app would pick, best first |
| `required` | boolean, default `true` | If `false`, the app works without this role |
| `label` | string | Name shown in Local Router, for example "Chapters" |

A **model reference** names a model in a store without the string syntax:

```json
{ "store": "ollama", "id": "qwen3.5:9b" }
{ "store": "hf", "id": "ggerganov/whisper.cpp", "file": "ggml-large-v3-turbo-q8_0.bin", "revision": "main" }
```

Four role names are well-known and have a global binding that the user sets once for all apps:

| Role | Capability | Typical model |
|---|---|---|
| `chat` | `chat` | The default model starred in Local Router |
| `embed` | `embedding` | An embedding model |
| `transcribe` | `transcription` | Whisper |
| `diarize` | `diarization` | A speaker-diarization pipeline |

Apps MAY declare their own role names, such as `chapters`. An app can use the four well-known roles without declaring them.

### 6.2 Resolving a role

When a request names `localrouter/<role>`, `localrouterd` picks the model in this order:

1. **The binding the user set for this app and this role.** If that model is no longer installed, the request fails with `409 model_not_downloaded`. Local Router doesn't silently use a different model than the one the user chose.
2. **The global binding** of the role with the same name, if it is one of the well-known roles. Otherwise, the global binding of the well-known role with the same capability; for example `chapters` (capability `chat`) falls back to `chat`.
3. **The app's own suggestions,** in order: the first one that is installed and fits.

A model **fits** when it has the role's capability, Local Router can run its format, and its context length is at least `min_context` if that is set and the model's context length is known. `languages` is only a hint and never rules a model out.

A role the app hasn't declared, and isn't well-known, fails with `409 role_unbound`. So does a role for which nothing fits. The error's `details` carry the role and a link, `localrouter://apps?app=<app_id>&role=<role>`, that the app SHOULD offer as "Choose a model in Local Router".

A request that names a concrete model id that Local Router doesn't know gets `404 not_found`. If the model is known but its files are missing, it gets `409 model_not_downloaded`.

The answer to a request that used a role says which model was used: in the `LR-Resolved-Model` header, and in the response's `model` field where the format has one.

`GET /lr/v1/me` returns the app's roles with their current bindings. When a binding that affects an app changes, including a global binding, that app receives `role.bound` or `role.unbound` for each of its roles that now resolves differently.

## 7. Models

### 7.1 The inventory

`GET /lr/v1/models` lists the models Local Router knows about, from all stores. It takes optional filters: `kind`, `store`, `state`. The answer is `{ "models": [ … ] }`. A model has these fields:

| Field | Notes |
|---|---|
| `id` | Model id (section 2) |
| `store` | `ollama`, `hf` or `app` |
| `kind` | `llm`, `embedding`, `asr`, `diarization`, `vad`, `denoise` or `other` |
| `display_name` | Short readable name, for example "Whisper Large v3 Turbo" |
| `maker` | Optional, for example `openai` |
| `format` | `gguf`, `mlx`, `ggml`, `safetensors`, `coreml`, `onnx` or `other` |
| `size_bytes` | Size on disk |
| `quantization`, `parameters` | Optional, for example `q8_0`, `8B` |
| `context_length` | Optional, in tokens |
| `capabilities` | For example `chat`, `embedding`, `transcription`, `diarization`, `vision`, `tools`, `thinking`, `audio` |
| `languages` | Optional list |
| `state` | `available`, `downloading`, `loading`, `loaded` or `missing` (declared by an app but not on disk) |
| `loaded` | When loaded: `memory_bytes`, `context`, `expires_at`, `held_by_you` (whether the caller holds it), `holders` (how many apps hold it) |
| `bound_for_you` | Roles of the caller that are bound to this model |
| `in_ollama` | For `hf` GGUF files: whether the same file has been added to Ollama |

An app learns only about itself: which roles it bound and whether it holds a model, never which other apps exist. Models that other apps run themselves (store `app`) appear for other apps only as memory in use, without the app's name: `id` is `app:other`. Local Router's own window sees everything.

Hugging Face repositories that Local Router doesn't recognise are listed with `kind: "other"`. Local Router doesn't run them.

### 7.2 Holding a model

An app that wants a model ready, for example before the user presses Record, asks Local Router to **hold** it:

```http
POST /lr/v1/models/load
{ "model": "localrouter/transcribe", "context": 32768, "keep_alive": "15m" }
```

A hold belongs to the app's identity and ends at the first of:
- `POST /lr/v1/models/unload` for that model;
- disconnecting (`DELETE /lr/v1/me`);
- the app's last open event stream closing (section 10), for apps that hold one;
- `keep_alive` passing without a new load request. The default is `15m`, and the longest allowed is `4h`.

While at least one app holds a model, Local Router keeps it loaded. It unloads it when the last hold ends, unless the user pinned it or a schedule keeps it.

`load` answers `200` when the model is loaded, or `202` if loading continues in the background, with progress in `model.state` events.

**Context length.** A language model is loaded with the largest `context` any holder asked for. If a request needs more context than the model is loaded with, Local Router reloads it, unless another app holds the model. In that case the request fails with `409 context_in_use`, and `details` carry `loaded_context` and `requested_context`.

Apps cannot delete models. Deleting is up to the user, in Local Router.

## 8. Downloads

`POST /lr/v1/downloads` asks Local Router to download a model into its standard place. The model's `source` is one of these:

| `source` | Use it for | Where it goes |
|---|---|---|
| `ollama` | Ollama library models, `{ "store": "ollama", "id": "qwen3.5:9b" }` | Ollama's store, through Ollama's pull |
| `hf` | Hugging Face repositories or files, downloaded from huggingface.co | The Hugging Face cache |
| `mirror` | Private models of the app's own maker, hosted elsewhere | The Hugging Face cache, in the same layout as `hf` |

For `mirror`, the app supplies the files and the Hugging Face revision they belong to, so they end up exactly where `huggingface_hub` would put them:

```json
{
  "model": { "store": "hf", "id": "askantis/gruezi-v1" },
  "source": "mirror",
  "mirror": {
    "revision": "0123456789abcdef0123456789abcdef01234567",
    "files": [
      { "path": "ggml-model-q8_0.bin", "url": "https://example.blob.core.windows.net/models/…?sig=…",
        "size_bytes": 874000000, "sha256": "…", "etag": "…" }
    ]
  }
}
```

The answer is `202` with a download record:

```json
{ "id": "dl_7Q2…", "model": "hf:askantis/gruezi-v1", "source": "mirror", "state": "running",
  "bytes_done": 0, "bytes_total": 874000000, "started_by_you": true }
```

A download's `state` moves through `queued`, `running` and `verifying`, and ends as `done`, `failed` or `cancelled`.
- `GET /lr/v1/downloads` lists the caller's own downloads.
- `GET /lr/v1/downloads/{id}` returns one of them.
- `DELETE /lr/v1/downloads/{id}` cancels it. An app can cancel only downloads it started.
- If two apps ask for the same model, they share one download, and each sees it as its own.

Progress is announced with `download.progress` events, at most 4 a second per download, followed by `download.done` or `download.failed`.

### 8.1 Rules for mirror downloads

A mirror lets an app write into a cache that other programs trust, so `localrouterd` MUST enforce all of the following:

1. **Who.** Only `askantis` and `signed` apps can use `mirror`. Each signing team is mapped to the Hugging Face organisations it owns (Askantis: `askantis`). The repository MUST belong to the caller's organisation; otherwise the answer is `403 forbidden`. Other publishers can ask Askantis to be added.
2. **Never over someone else's files.** If the repository already exists in the cache, the mirror MUST NOT change `refs/main` or add files to a snapshot unless every existing file there matches the declared `sha256`. If it doesn't match, it fails with `409 conflict` and Local Router tells the user.
3. **Paths.** Every `path` is split at `/`. Each part MUST be non-empty, MUST NOT be `.` or `..`, and MUST NOT contain `\`, `:` or control characters. Files are created without following symbolic links, and only inside `models--<org>--<repo>`. `etag` MUST be 40 or 64 hexadecimal characters, and for files of 10 MB or more it MUST equal `sha256`.
4. **URLs.** Only `https`. The host, after DNS resolution and after every redirect, MUST NOT be a loopback, link-local, private-network (RFC 1918, RFC 4193) or `.local` address. A file larger than its `size_bytes` is cut off and fails.
5. **Checks.** Every file is checked against its `sha256` before it becomes visible.
6. **Secrets.** URLs, signatures and tokens supplied for a download are used for that download only, and MUST NOT be written to disk or logs.

### 8.2 Rules for every download into the Hugging Face cache

- The result is the standard layout: `blobs/<etag>`; `snapshots/<revision>/<path>` as a relative symbolic link to the blob, or a copy where symbolic links aren't available; and `refs/main`.
- Files are written to `<blob>.incomplete` first and resumed with HTTP Range requests. They take the same lock files as `huggingface_hub`: `.locks/models--<org>--<repo>/<etag>.lock`.
- A model that is already present is not downloaded again: the answer is a download record that is already `done`.
- Local Router MUST NOT use a Hugging Face token stored on the computer (`HF_TOKEN`, `~/.cache/huggingface/token`) for a download an app started, unless the user agrees in Local Router. An app can supply its own `token` for gated repositories instead; it is used once.
- A download started by an app shows in Local Router's window with the app's name, so the user always sees what is being downloaded.

## 9. Models that apps run themselves

An app running standalone, or using a model Local Router can't run, can still tell Local Router about it, so the memory shown in Local Router is true:

```http
PUT /lr/v1/me/resident
Content-Type: application/json

{ "models": [
    { "name": "whisper", "kind": "asr", "display_name": "Whisper Large v3 Turbo",
      "memory_bytes": 1650000000, "state": "loaded", "device": "gpu" }
  ],
  "lease_seconds": 60 }
```

The list replaces the previous one. The models appear as `app:<app_id>/<name>` in the inventory. The report ends at the earliest of:
- the moment the last of the app's event streams (section 10) closes;
- `lease_seconds` after the report, unless the app sends it again.

When memory runs short, or when the user asks, Local Router MAY send that app an `unload_requested` event naming the model. The app SHOULD unload the model when it isn't using it, and report the new list.

## 10. Events

`GET /lr/v1/events` is a stream of [server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html):

```
id: 1042
event: download.progress
data: {"id":"dl_7Q2…","bytes_done":104857600,"bytes_total":874000000}
```

- Event ids increase within one `instance_id`. A client that reconnects sends `Last-Event-ID` and receives what it missed, from a buffer of the last 1000 events. If the buffer no longer reaches back that far, or Local Router restarted, the first event is `reset`, and the client fetches the state it cares about again.
- A comment line (`: keep-alive`) is sent every 15 seconds.
- Only connected apps (and key clients) can open the stream.
- Clients MUST ignore event types they don't know.

| Event | Who receives it | `data` |
|---|---|---|
| `reset` | everyone | `{ "instance_id" }` |
| `shutdown` | everyone | `{ "restarting": true \| false }` |
| `ollama.state` | everyone | `{ "state", "version"? }` |
| `hf_cache.state` | everyone | `{ "path", "state" }` |
| `memory.pressure` | everyone | `{ "level": "normal" \| "warning" \| "critical" }` |
| `model.added`, `model.removed` | everyone | `{ "model": Model }` / `{ "id" }`, filtered as in section 7.1 |
| `model.state` | everyone | `{ "id", "state", "memory_bytes"?, "progress"? }` |
| `download.progress` | apps that started that download | `{ "id", "bytes_done", "bytes_total" }` |
| `download.done`, `download.failed` | apps that started that download | `{ "download": Download }` |
| `role.bound`, `role.unbound` | the affected app | `{ "role", "model"? }` |
| `unload_requested` | the app that reported the model | `{ "model", "reason": "memory_pressure" \| "user" }` |
| `connect.updated` | Local Router's window | `{ "request_id", "status" }` |
| `activity` | Local Router's window | `{ "app_id", "model", "kind": "chat" \| "embedding" \| "transcription" \| "diarization", "phase": "started" \| "finished", "tokens"?, "audio_seconds"? }` |

## 11. Inference

### 11.1 Choosing the model

Every inference request names a model: a concrete id, or `localrouter/<role>`. `GET /v1/models` lists what the caller can use, in OpenAI's format: the concrete models, plus one `localrouter/<role>` entry for each of the caller's bound roles.

For compatibility with existing tools, the OpenAI model names `whisper-1`, `gpt-4o-transcribe` and `gpt-4o-mini-transcribe` mean `localrouter/transcribe`.

Ollama itself, on port 11434, has no authentication. Connecting to Local Router is the user's consent for an app to use models through Local Router. It does not isolate apps from Ollama.

### 11.2 Chat and embeddings

`POST /v1/chat/completions` and `POST /v1/embeddings` follow the OpenAI API, including streaming. `localrouterd` runs them on Ollama and controls two things that Ollama's own OpenAI layer can't:

- **Context length.** The context is, in order:
  1. the request's `lr_context`;
  2. otherwise the role's `min_context`;
  3. otherwise the context the user set for that model in Local Router;
  4. otherwise Ollama's default.

  Section 7.2 applies when the model is already loaded with a different context. The context used is returned in the `LR-Context-Length` header.
- **Keep-alive.** The request's `lr_keep_alive`, limited to the range allowed in section 7.2. Otherwise Local Router's setting.

Structured output (`response_format` of type `json_schema` or `json_object`) is honoured. Models that think can be told not to with `"think": false`. Usage counts are returned when `stream_options.include_usage` is set, as in OpenAI's API.

A prompt longer than the context MUST NOT be cut silently. When the runtime reports that the prompt filled the context, the answer is `400 context_length_exceeded`, with `details.context_length`. In a stream that has already started, it is the `error` event described in section 11.4.

### 11.3 Ollama's API

Only these Ollama endpoints are available, passed to Ollama and streamed back:

| Endpoint | Notes |
|---|---|
| `POST /api/chat`, `POST /api/generate` | `localrouter/<role>` resolved; `options.num_ctx` filled in from section 11.2 if missing; `keep_alive` limited as in section 7.2 |
| `POST /api/embed`, `POST /api/embeddings` | `localrouter/<role>` resolved |
| `GET /api/tags`, `POST /api/show`, `GET /api/ps`, `GET /api/version` | Unchanged |

Every other `/api/*` path answers `403 forbidden`. That includes pulling, pushing, creating, copying and deleting models, and uploading blobs. Apps download through `/lr/v1/downloads` (section 8). Answers and errors keep Ollama's own shape, for example `{"error": "…"}`.

### 11.4 Speech to text

`POST /v1/audio/transcriptions` follows OpenAI's API, as `multipart/form-data`.

| Field | Notes |
|---|---|
| `file` | The audio. WAV, MP3, M4A/AAC, FLAC, Ogg or WebM with Opus, and the audio track of MOV/MP4. At most 2 GiB and 4 hours. |
| `model` | A model id, `localrouter/transcribe`, or `whisper-1` |
| `language` | ISO 639-1 code, or `gsw` for Swiss German. Omitted: detected. |
| `prompt` | Optional text to guide spelling (names, terms) |
| `response_format` | `json` (default), `text`, `srt`, `vtt`, `verbose_json` or `lr_speakers_json` |
| `timestamp_granularities[]` | `segment` and/or `word`, for `verbose_json` and `lr_speakers_json` |
| `temperature` | Default `0` |
| `stream` | `true` for server-sent events (below). Only with `json`, `verbose_json` and `lr_speakers_json`. |

Local Router extensions; implementations MUST ignore unknown `lr_` fields:

| Field | Notes |
|---|---|
| `lr_diarize` | `true` to separate speakers. Implied by `lr_speakers_json`. |
| `lr_num_speakers` / `lr_min_speakers` / `lr_max_speakers` | Hints for speaker separation |
| `lr_segments` | JSON array of `{ "start", "end", "speaker"? }` in seconds. Transcribe only these stretches of the file, for example the turns an app got from `/lr/v1/audio/diarizations`. Times in the answer stay in the timeline of the whole file, and each segment keeps its `speaker`. |
| `lr_offset` | Seconds to add to every time in the answer. For live transcription, where each request is a short window of a longer recording. |
| `lr_denoise` | `true` to reduce background noise before transcribing |
| `lr_vad` | `false` to skip voice-activity detection. Default `true`. |
| `lr_priority` | `live`, `interactive` (default) or `batch`. Live requests go first, so a live recording never waits behind a two-hour file. |

The audio is processed in memory and never stored (section 14). A file path can't be sent instead of the audio, because the service must not read the user's files. If the client closes the connection, the job is cancelled.

**`verbose_json`** has OpenAI's shape:

```json
{
  "task": "transcribe", "language": "de", "duration": 12.48,
  "text": "Hoi Thomas, wie gaht's?",
  "segments": [ { "id": 0, "start": 0.32, "end": 2.10, "text": "Hoi Thomas, wie gaht's?",
                  "avg_logprob": -0.21, "no_speech_prob": 0.01,
                  "words": [ { "word": "Hoi", "start": 0.32, "end": 0.58, "probability": 0.97 } ] } ],
  "words": [ { "word": "Hoi", "start": 0.32, "end": 0.58, "probability": 0.97 } ]
}
```

`words` appear only when `word` granularity was asked for.

**`lr_speakers_json`** adds speakers. It is Local Router's own format; OpenAI's `diarized_json` has a different shape and is reserved for a later version.

```json
{
  "task": "transcribe", "language": "de", "duration": 12.48, "text": "…",
  "speakers": [ { "id": "S1" }, { "id": "S2" } ],
  "segments": [ { "id": 0, "start": 0.32, "end": 2.10, "speaker": "S1", "text": "…",
                  "words": [ { "word": "Hoi", "start": 0.32, "end": 0.58, "probability": 0.97 } ] } ],
  "overlaps": [ { "start": 5.10, "end": 5.60, "speakers": ["S1", "S2"] } ]
}
```

Speakers are numbered `S1`, `S2`, … in the order they first speak. Naming them is up to the app. With `lr_segments`, the speakers are the ones the app supplied.

**Streaming** (`stream=true`) sends these events, then closes the stream. Every `data` object carries its event name in a `type` field, as in OpenAI's API:

| Event | `data` |
|---|---|
| `transcript.text.delta` | `{ "type", "delta" }` |
| `transcript.text.segment` | `{ "type", "segment" }`: one finished segment. Segments that were sent don't change later. |
| `transcript.text.done` | `{ "type", … }`: the complete response, in the requested format |
| `error` | `{ "type": "error", "error": { … } }`: the error object of section 12; the stream then ends |

**Live transcription** is a series of short requests. The app records the audio itself, so the microphone permission belongs to the app. Every few seconds it sends the newest stretch of audio, up to 30 seconds, as WAV with `lr_priority=live` and `lr_offset` set to where that stretch starts in the recording. Windows MAY overlap; the app keeps the words with the latest start time for each moment. Version 1 has no session API, and separating speakers in live mode is left to a later version.

### 11.5 Speaker diarization

`POST /lr/v1/audio/diarizations` separates speakers without transcribing. It is `multipart/form-data` with:
- `file`;
- `model` (default `localrouter/diarize`);
- `num_speakers`, `min_speakers`, `max_speakers`;
- `response_format`: `json` (default) or `rttm`;
- `stream`.

The JSON answer:

```json
{
  "duration": 3600.0,
  "speakers": [ { "id": "S1" }, { "id": "S2" } ],
  "segments": [ { "start": 0.32, "end": 4.75, "speaker": "S1" } ],
  "overlaps": [ { "start": 5.10, "end": 5.60, "speakers": ["S1", "S2"] } ]
}
```

`segments` don't overlap: each moment belongs to at most one speaker. Overlapping speech is listed separately in `overlaps`.

With `stream=true`, the answer is a stream of server-sent events:
- `diarization.progress` with `{ "type", "done_seconds", "total_seconds" }`;
- then `diarization.done` with `{ "type", "result" }`;
- or `error`.

Closing the connection cancels the job.

An app that runs its own pipeline (separate speakers, then transcribe each turn) uploads the file twice:
1. once to this endpoint;
2. once to `/v1/audio/transcriptions` with `lr_segments` set to the turns it got.

## 12. Errors

`/lr/v1/*` and `/v1/*` answer errors in one shape:

```json
{ "error": { "type": "invalid_request_error", "code": "role_unbound",
             "message": "No model is chosen for “chapters”.", "param": null,
             "details": { "role": "chapters", "link": "localrouter://apps?app=app.askantis.scribely&role=chapters" } } }
```

`message` is meant for people, in English. Apps SHOULD show their own translated text, based on `code`.

| Status | `code` | `type` | When |
|---|---|---|---|
| 400 | `invalid_request` | `invalid_request_error` | Malformed request; `param` names the field |
| 400 | `context_length_exceeded` | `invalid_request_error` | The prompt doesn't fit the context; `details.context_length` |
| 401 | `unauthorized` | `authentication_error` | TCP request without a valid key |
| 403 | `not_approved` | `permission_error` | The app isn't connected, or the connection was removed |
| 403 | `identity_mismatch` | `permission_error` | `app_id` doesn't match the code signature, or belongs to a signed app |
| 403 | `forbidden` | `permission_error` | Not allowed for this caller (for example an Ollama endpoint outside the list, a mirror outside the caller's organisation, an interpreter connecting, a web page) |
| 404 | `not_found` | `not_found_error` | Unknown path, model, download or request id |
| 409 | `role_unbound` | `invalid_request_error` | Section 6.2; `details.link` |
| 409 | `model_not_downloaded` | `invalid_request_error` | The model isn't on disk; `details.model` |
| 409 | `context_in_use` | `invalid_request_error` | Section 7.2; `details.loaded_context`, `details.requested_context` |
| 409 | `conflict` | `invalid_request_error` | A mirror would change files that are already in the cache (section 8.1) |
| 413 | `payload_too_large` | `invalid_request_error` | Upload over the limit |
| 415 | `unsupported_media_type` | `invalid_request_error` | Wrong content type, or audio format that can't be decoded |
| 429 | `busy` | `rate_limit_error` | Queue full; `Retry-After` header |
| 500 | `internal` | `server_error` | A bug in Local Router |
| 502 | `upstream_unavailable` | `upstream_error` | Ollama isn't running or didn't answer |
| 503 | `model_loading` | `server_error` | The model is still loading; `Retry-After` header |
| 503 | `insufficient_memory` | `server_error` | Not enough memory to load the model; `details.needed_bytes`, `details.available_bytes` |

`/api/*` keeps Ollama's error shape (section 11.3).

## 13. Versioning

- The discovery file and `GET /lr/v1/health` list the protocol versions the service speaks in `protocols`, for example `[1]` or `[1, 2]`. This document describes version 1.
- Within `/lr/v1`, changes only add things: new endpoints, new optional fields, new event types, new error codes. Clients MUST ignore fields and event types they don't know.
- A change that would break clients gets a new path prefix, `/lr/v2`. Local Router then serves `/lr/v1` alongside `/lr/v2` for at least two further releases.
- `GET /lr/v1/health` lists `capabilities`. Clients SHOULD check for a capability, not for a version number, before using an optional feature.

## 14. Privacy rules for implementations

These rules keep Local Router's promise that it collects nothing. An implementation of `localrouterd`:

1. MUST NOT write prompts, answers, audio, transcripts or embeddings to disk, to logs, or anywhere else. They exist in memory only while a request runs.
2. MAY record metadata about requests (time, app, endpoint, model, duration, token counts, audio length, bytes) and show it to the user. It MUST NOT send that metadata anywhere.
3. MUST NOT open network connections on its own. The only connections it makes are:
   - to Ollama on loopback;
   - for downloads the user or a connected app asked for, which are always shown to the user.
4. MUST NOT send telemetry, analytics or crash reports, and MUST turn off telemetry in libraries it uses.
5. MUST listen on loopback and on its Unix socket only (section 3.1).
6. MUST NOT read files on behalf of apps. Apps upload the data a request needs.
7. MUST NOT tell one app which other apps use Local Router (sections 7.1 and 10).

Apps that follow this protocol SHOULD make the same promises for their own part.

## 15. Rules for client libraries

A library that implements the client side, such as the Rust crate `localrouter-client`:

1. Finds Local Router as described in section 4.2, and never blocks the app's start-up for longer than it takes to read the discovery file.
2. Uses the Unix socket where the platform has one, and TCP with a key otherwise.
3. Offers the app one way to run a job, whether Local Router is there or not. In standalone mode it runs the same engine inside the app and uses the same standard places.
4. Keeps one event stream open while the app is connected. Holds and resident-model reports end when it closes.
5. Reconnects to the event stream with `Last-Event-ID`, backing off 1, 2 and 4 seconds, then at most 30.
6. Respects `Retry-After`.
7. Turns `role_unbound` and `model_not_downloaded` into something the app can show: the link to Local Router, or a download.
8. Reports resident models (section 9) when the app runs models itself while connected.
9. On `shutdown` with `restarting: true`, waits for the new discovery file. Otherwise it switches to standalone mode until Local Router is back.
10. Stores an `lra_` key, if it has one, in the operating system's keychain, never in plain files.

## 16. Security considerations

- **Web pages.** Section 3.3 blocks DNS rebinding and cross-site requests. Implementations MUST test these rules.
- **Process ids are not identities.** On TCP, mapping a connection to a process id is unreliable: ids get reused and sockets get inherited. Trust decisions MUST use the identity the operating system gives for the Unix socket (section 5.1), never a process id.
- **Code injection.** An app that can be injected into (section 5.1) is treated as `unsigned`, so another program can't borrow its approval by loading code into it.
- **Keys.** `lra_` keys have at least 256 bits of randomness. Only their SHA-256 hashes are stored, and they are compared in constant time.
- **Approval prompts** show what the code signature says, never only what the app claims (section 5.2).
- **The shared cache.** Several programs may write to the Hugging Face cache at once. Implementations MUST use the same lock files as `huggingface_hub`, MUST follow section 8.1 for mirrors, and MUST NOT delete a model that a connected app has declared without asking the user.
- **Apps can't spy on each other.** Inventory, downloads and events are filtered per app (sections 7.1, 8 and 10).

## 17. Platform notes

- **macOS.**
  - `localrouterd` is a launch agent inside `Local Router.app`. The app registers it with `SMAppService`, and it keeps running when the menu-bar app is closed.
  - Launching the app in the background (`open -g -b ch.askantis.localrouter`) starts it. The bundle identifier of Local Router is `ch.askantis.localrouter`.
  - Sandboxed apps (for example from the Mac App Store) can't reach the socket, the discovery file or the Hugging Face cache, so version 1 doesn't support them.
- **Windows** (planned). TCP listener as on macOS. A named pipe `\\.\pipe\localrouter` replaces the Unix socket; the caller is identified with `GetNamedPipeClientProcessId` and its Authenticode signature. Discovery file in `%LOCALAPPDATA%\Askantis\Local Router\`. The Hugging Face cache is `%USERPROFILE%\.cache\huggingface\hub` unless the variables in section 2 say otherwise.
- **Linux** (not planned yet). Unix socket in `$XDG_RUNTIME_DIR/localrouter/`, caller identified with `SO_PEERCRED`. Without code signatures, every app needs the user's approval.
