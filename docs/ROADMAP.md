# Roadmap

Local Router 1.0 is a menu-bar companion for Ollama: see what's loaded, load and eject models, watch memory and context, and download models that fit your Mac.

The name comes from where it's going: a local, private router in the spirit of OpenRouter. Your apps talk to one endpoint on your Mac, and Local Router decides which local model answers.

## Next: a local endpoint

A background service on your Mac that speaks both the OpenAI and Ollama APIs. Apps connect to it with one click, and it runs speech models (transcription, telling speakers apart) as well as handing language models to Ollama. The protocol is drafted in [spec/PROTOCOL.md](../spec/PROTOCOL.md). Point your tools at it instead of Ollama, and Local Router can see what each model is doing:

- **Live status in the menu bar:** reading a prompt, thinking, calling tools or looking at an image.
- **Per-app identity:** which app or agent uses which model, with its own key.
- **Fixed context sizes:** every request uses the same `num_ctx`, so models don't reload when two apps ask for different sizes.
- **No silent truncation:** a clear error when a prompt is bigger than the context, instead of Ollama quietly dropping old messages.

## Later: routing

- **Tags as virtual models.** Ask for `coding` or `writing` and get the model you tagged for it.
- **An always-on small model** that answers quick questions itself and hands harder ones to a bigger model.
- **Sticky sessions,** so a conversation stays on the model that holds its cache.
- **A memory budget in GB** with pinning, least-recently-used eviction and preloading.
- **Per-app allowlists and limits.**

## Ideas

- Agent integrations (one-click setup for tools that support custom OpenAI endpoints)
- Usage history and speed stats per model
- In-app updates

Have an idea or a use case? [Open an issue](https://github.com/Askantis/public-local-router/issues).
