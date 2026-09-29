import Foundation

/// Finds a running Ollama: the address in OLLAMA_HOST, the ports Ollama
/// itself listens on, then the default. Returns the first that answers.
enum OllamaFinder {
    static let defaultAddress = "http://localhost:11434"

    static func find() async -> (address: String, version: String)? {
        for candidate in await Task.detached(operation: candidates).value {
            if let version = await version(at: candidate) { return (candidate, version) }
        }
        return nil
    }

    private static func candidates() -> [String] {
        var result: [String] = []
        let fromLaunchd = OllamaRunner.shell("/bin/launchctl", ["getenv", "OLLAMA_HOST"]).trimmingCharacters(in: .whitespacesAndNewlines)
        for host in [fromLaunchd, ProcessInfo.processInfo.environment["OLLAMA_HOST"] ?? ""] where !host.isEmpty {
            result.append(normalize(host))
        }
        // Ports the `ollama` process listens on (its llama-server children
        // have their own name, so they're not included).
        let listening = OllamaRunner.shell("/usr/sbin/lsof", ["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-c", "ollama", "-Fn"])
        for line in listening.split(separator: "\n") where line.hasPrefix("n") {
            if let port = line.split(separator: ":").last { result.append("http://127.0.0.1:\(port)") }
        }
        result.append(defaultAddress)
        return result.reduce(into: []) { if !$0.contains($1) { $0.append($1) } }
    }

    /// OLLAMA_HOST can be "0.0.0.0", ":11434", "host:port" or a full URL.
    private static func normalize(_ host: String) -> String {
        var value = host
        if !value.contains("://") { value = "http://" + value }
        guard var components = URLComponents(string: value) else { return defaultAddress }
        if components.host == nil || components.host == "" || components.host == "0.0.0.0" || components.host == "*" {
            components.host = "127.0.0.1"
        }
        if components.port == nil { components.port = 11434 }
        return components.string ?? defaultAddress
    }

    private static func version(at address: String) async -> String? {
        guard let url = URL(string: address) else { return nil }
        return try? await OllamaClient(baseURL: url).version()
    }
}
