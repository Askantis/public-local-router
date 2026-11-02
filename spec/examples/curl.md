# Example requests

These use `curl`. On the Unix socket the host name doesn't matter, so the examples use `http://lr`.

```bash
SOCK="$HOME/Library/Application Support/Local Router/lr.sock"
```

**Is Local Router running?**

```bash
curl -s --unix-socket "$SOCK" http://lr/lr/v1/health
```

**Connect an app** (on the socket only; `curl` is one of Apple's command-line tools, so it counts as unsigned: it gets `202 pending` and a prompt in Local Router):

```bash
curl -s --unix-socket "$SOCK" -H 'Content-Type: application/json' \
  -d @spec/examples/connect-request.json http://lr/lr/v1/connect
```

**Roles and their models**

```bash
curl -s --unix-socket "$SOCK" http://lr/lr/v1/me
```

**Transcribe with speakers**

```bash
curl -s --unix-socket "$SOCK" http://lr/v1/audio/transcriptions \
  -F file=@interview.m4a -F model=localrouter/transcribe -F language=gsw \
  -F response_format=lr_speakers_json -F 'timestamp_granularities[]=word' -F lr_max_speakers=4
```

**Chat with the model bound to a role**

```bash
curl -s --unix-socket "$SOCK" -H 'Content-Type: application/json' http://lr/v1/chat/completions \
  -d '{"model":"localrouter/chapters","messages":[{"role":"user","content":"Hoi!"}],"lr_context":32768}'
```

**Over TCP, with a key** (for tools that can't use the socket; read the port from `endpoint.json`)

```bash
curl -s http://127.0.0.1:11436/v1/models -H "Authorization: Bearer $LRA_KEY"
```

**Follow events**

```bash
curl -sN --unix-socket "$SOCK" http://lr/lr/v1/events
```
