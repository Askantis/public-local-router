# Local Router Protocol

How apps connect to Local Router and use the models it manages.

| File | What it is |
|---|---|
| [PROTOCOL.md](PROTOCOL.md) | The specification, version 1 (draft). Start here. |
| [openapi.yaml](openapi.yaml) | The endpoints and payloads, as OpenAPI 3.1 |
| [schemas/endpoint.schema.json](schemas/endpoint.schema.json) | JSON Schema of the discovery file |
| [examples/](examples/) | Example payloads, checked against the schemas; [curl.md](examples/curl.md) shows requests |
| [tools/check.rb](tools/check.rb) | Consistency check: `ruby spec/tools/check.rb` |

In short: Local Router's service, `localrouterd`, listens on a Unix socket and on `127.0.0.1:11436`. It writes `~/Library/Application Support/Local Router/endpoint.json` when it's ready. Apps connect once with `POST /lr/v1/connect`, declare the roles they need models for (`transcribe`, `chat`, …), and then call OpenAI- or Ollama-compatible endpoints with `"model": "localrouter/<role>"`. Models stay in their standard places: Ollama's store and the Hugging Face cache.

Apps must keep working without Local Router. See section 4.2 of the specification.

## Status

Draft 1. The endpoints may still change until `localrouterd` 2.0 ships. After that, version 1 only grows (section 13).

## License

The specification, like the rest of this repository, is licensed under the PolyForm Noncommercial License 1.0.0 or the PolyForm Small Business License 1.0.0, at your choice, once the repository's license change lands. Until then, [LICENSE](../LICENSE) applies. Larger companies: [contact Askantis](https://askantis.ch).
