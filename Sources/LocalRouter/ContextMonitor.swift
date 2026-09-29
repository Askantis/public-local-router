import AppKit
import Foundation

/// How full a loaded model's context is. Ollama doesn't report this, but each
/// GGUF model runs in its own llama.cpp `llama-server`, whose `/slots`
/// endpoint says how many tokens the last request used.
struct ContextUsage: Equatable, Sendable {
    let size: Int
    let promptTokens: Int
    /// Prompt tokens reused from the cache plus those processed so far.
    let promptDone: Int
    let decoded: Int
    let isProcessing: Bool

    var used: Int { promptTokens + decoded }
    var fraction: Double { size > 0 ? min(1, Double(used) / Double(size)) : 0 }
}

/// What a loaded model is doing right now, as far as llama-server tells.
/// Thinking, tool calls and images aren't visible from here; that needs
/// requests to pass through Local Router (the V2 proxy).
enum ModelActivity: Equatable {
    case idle
    /// Processing the prompt ("prefill"), the slow part with long chats.
    case reading(done: Int, total: Int)
    /// Generating tokens; thinking or answering. Shown in green.
    case writing(tokens: Int, perSecond: Double?)

    var color: NSColor? {
        switch self {
        case .idle: nil
        case .reading: .systemOrange
        case .writing: .systemGreen
        }
    }
}

extension ModelStore {
    /// Matches loaded models to their llama-server processes and reads `/slots`.
    /// MLX models don't use llama-server, so they get no entry.
    func updateContextUsage() async {
        guard !loaded.isEmpty else {
            if !contextUsage.isEmpty { contextUsage = [:] }
            if !activity.isEmpty { activity = [:] }
            return
        }
        let runners = await Task.detached { ContextMonitor.ollamaRunners() }.value
        var usage: [String: ContextUsage] = [:]
        var states: [String: ModelActivity] = [:]
        let now = Date.now
        for model in loaded {
            guard let blob = await blobName(for: model.name),
                  let port = runners[blob],
                  let slot = await ContextMonitor.slots(port: port) else { continue }
            usage[model.name] = slot
            states[model.name] = activity(from: slot, model: model.name, at: now)
        }
        if usage != contextUsage { contextUsage = usage }
        if states != activity { activity = states }
    }

    private func activity(from slot: ContextUsage, model: String, at now: Date) -> ModelActivity {
        guard slot.isProcessing else {
            decodeSamples[model] = nil
            return .idle
        }
        // Once the whole prompt is processed, it's generating, even before the
        // first token is counted.
        if slot.decoded == 0 && slot.promptDone < slot.promptTokens {
            return .reading(done: slot.promptDone, total: slot.promptTokens)
        }
        // Speed from the change since the last poll.
        var speed: Double?
        if let last = decodeSamples[model], slot.decoded > last.tokens {
            speed = Double(slot.decoded - last.tokens) / now.timeIntervalSince(last.time)
        }
        decodeSamples[model] = (slot.decoded, now)
        return .writing(tokens: slot.decoded, perSecond: speed)
    }

    /// The model's weights file (`sha256-…`), from the FROM line of its Modelfile.
    private func blobName(for model: String) async -> String? {
        if let cached = modelBlobs[model] { return cached }
        guard let modelfile = try? await client.modelfile(model),
              let from = modelfile.split(separator: "\n").first(where: { $0.hasPrefix("FROM ") }) else { return nil }
        let blob = URL(fileURLWithPath: String(from.dropFirst(5))).lastPathComponent
        modelBlobs[model] = blob
        return blob
    }
}

enum ContextMonitor {
    /// Ollama's llama-server processes: weights file name → port.
    /// Hermes and other apps run their own llama-server; those are skipped.
    static func ollamaRunners() -> [String: Int] {
        let output = OllamaRunner.shell("/bin/ps", ["-Ao", "args"])
        var result: [String: Int] = [:]
        // Match the blob name and port directly: paths can contain spaces
        // (e.g. an external drive called "Extreme SSD").
        for line in output.split(separator: "\n") where line.contains("llama-server") && line.lowercased().contains("ollama") {
            let text = String(line)
            guard let blob = text.firstMatch(of: /sha256-[0-9a-f]{64}/),
                  let port = text.firstMatch(of: /--port (\d+)/).flatMap({ Int($0.1) }) else { continue }
            result[String(blob.0)] = port
        }
        return result
    }

    /// The busiest slot: one that's processing, else the fullest.
    static func slots(port: Int) async -> ContextUsage? {
        var request = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/slots")!)
        request.timeoutInterval = 1
        guard let (data, _) = try? await URLSession.shared.data(for: request),
              let slots = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]] else { return nil }
        return slots.map { slot -> ContextUsage in
            // `next_token` is a list in current llama.cpp, a single object in older builds.
            let next = (slot["next_token"] as? [[String: Any]])?.first ?? slot["next_token"] as? [String: Any]
            return ContextUsage(
                size: slot["n_ctx"] as? Int ?? 0,
                promptTokens: slot["n_prompt_tokens"] as? Int ?? 0,
                promptDone: (slot["n_prompt_tokens_cache"] as? Int ?? 0) + (slot["n_prompt_tokens_processed"] as? Int ?? 0),
                decoded: next?["n_decoded"] as? Int ?? 0,
                isProcessing: slot["is_processing"] as? Bool ?? false
            )
        }
        .max { ($0.isProcessing ? 1 : 0, $0.used) < ($1.isProcessing ? 1 : 0, $1.used) }
    }
}
