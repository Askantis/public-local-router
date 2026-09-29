# Privacy

**Local Router collects nothing.** No accounts, no analytics, no crash reports, no tracking, no telemetry.

## What stays on your Mac

- **Your prompts and answers.** Local Router never reads or stores them. It only reads Ollama's token counts and timings.
- **Your settings**, names, tags, favorites and access-key fingerprints, stored in the app's preferences on your Mac.
- **The Log Stream**, which reads Ollama's own log file on your Mac while the window is open, and keeps nothing afterwards.

## What Local Router connects to

- **Ollama on your Mac** (`localhost:11434`), to list, load, eject, download and delete models.
- **Nothing else by itself.** Model downloads are done by Ollama. The model catalog and the model makers' logos are bundled with the app. Links such as askantis.ch open in your browser only when you click them.

## Sharing

Sharing is off by default. When you turn on **Share with Access Keys**, Local Router accepts connections from your network on the port you choose, and only forwards requests with a valid access key to Ollama. Keys are shown once and stored only as SHA-256 fingerprints. With HTTPS on, the connection is encrypted with a certificate made on your Mac. Requests from other devices appear in the Log Stream, with their address and key name, and are not stored.

## Questions

Open an issue at https://github.com/Askantis/public-local-router or contact Askantis at https://askantis.ch.
