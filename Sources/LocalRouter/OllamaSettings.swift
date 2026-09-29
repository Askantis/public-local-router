import Foundation

/// Ollama's own server settings, which it reads from environment variables.
/// macOS has no config file for them, so Local Router keeps them, sets them
/// with `launchctl setenv` at every login (it's lost on reboot), and restarts
/// Ollama to apply a change. A nil value means "Ollama's default".
struct OllamaSettings: Codable, Equatable {
    var contextLength: Int?
    var kvCacheType: String?
    var flashAttention: Bool?
    var numParallel: Int?
    var maxLoadedModels: Int?
    var keepAlive: String?
    var exposeToNetwork = false

    /// Environment variable → value; nil means unset.
    var environment: [String: String?] {
        [
            "OLLAMA_CONTEXT_LENGTH": contextLength.map(String.init),
            "OLLAMA_KV_CACHE_TYPE": kvCacheType,
            "OLLAMA_FLASH_ATTENTION": flashAttention.map { $0 ? "1" : "0" },
            "OLLAMA_NUM_PARALLEL": numParallel.map(String.init),
            "OLLAMA_MAX_LOADED_MODELS": maxLoadedModels.map(String.init),
            "OLLAMA_KEEP_ALIVE": keepAlive,
            "OLLAMA_HOST": exposeToNetwork ? "0.0.0.0:11434" : nil,
        ]
    }

    static func load() -> OllamaSettings {
        guard let data = UserDefaults.standard.data(forKey: SettingsKey.ollamaServerSettings),
              let settings = try? JSONDecoder().decode(OllamaSettings.self, from: data) else { return OllamaSettings() }
        return settings
    }

    func save() {
        UserDefaults.standard.set(try? JSONEncoder().encode(self), forKey: SettingsKey.ollamaServerSettings)
    }

    /// Sets or clears each variable in launchd, so Ollama started from now on
    /// (the app, or the Homebrew service) picks them up.
    func applyToLaunchd() {
        for (key, value) in environment {
            if let value {
                OllamaRunner.shell("/bin/launchctl", ["setenv", key, value])
            } else {
                OllamaRunner.shell("/bin/launchctl", ["unsetenv", key])
            }
        }
    }
}

enum OllamaEnvironment {
    /// The OLLAMA_ variables the running `ollama` process was started with.
    static func running() -> [String: String]? {
        guard let pid = OllamaRunner.shell("/usr/bin/pgrep", ["-x", "ollama"]).split(separator: "\n").first else { return nil }
        let command = OllamaRunner.shell("/bin/ps", ["eww", "-p", String(pid), "-o", "command="])
        var result: [String: String] = [:]
        for part in command.split(separator: " ") where part.hasPrefix("OLLAMA_") {
            let pieces = part.split(separator: "=", maxSplits: 1).map(String.init)
            if pieces.count == 2 { result[pieces[0]] = pieces[1] }
        }
        return result
    }

    /// Variables the Homebrew service sets in its own plist; those win over launchd.
    static func homebrewOverrides() -> [String: String] {
        let plist = ["/opt/homebrew/opt/ollama/sh.brew.ollama.plist", "/usr/local/opt/ollama/sh.brew.ollama.plist"]
            .first(where: FileManager.default.fileExists)
        guard let plist, let dict = NSDictionary(contentsOfFile: plist),
              let environment = dict["EnvironmentVariables"] as? [String: String] else { return [:] }
        return environment
    }
}

extension ModelStore {
    var serverSettings: OllamaSettings {
        get { serverSettingsValue }
        set {
            serverSettingsValue = newValue
            newValue.save()
        }
    }

    /// Re-reads what Ollama actually runs with.
    func refreshRunningEnvironment() async {
        runningEnvironment = await Task.detached { OllamaEnvironment.running() }.value
    }

    /// Keys where Ollama runs with something other than what's saved.
    var pendingSettingKeys: [String] {
        guard let running = runningEnvironment else { return [] }
        let overrides = homebrewOverrides
        return serverSettings.environment.compactMap { key, wanted in
            if overrides[key] != nil { return nil } // Homebrew decides these
            return running[key] == wanted ? nil : key
        }
        .sorted()
    }

    /// Saves to launchd and restarts Ollama the way it was running.
    func applyServerSettings() async {
        serverSettings.applyToLaunchd()
        let runner = OllamaRunner.detect()
        applyingSettings = true
        defer { applyingSettings = false }
        runner.stop()
        for _ in 0..<20 {
            await refresh()
            if status == .offline { break }
            try? await Task.sleep(for: .milliseconds(500))
        }
        runner.start(environment: serverSettings.environment)
        for _ in 0..<30 {
            try? await Task.sleep(for: .milliseconds(500))
            await refresh()
            if case .running = status { break }
        }
        await refreshRunningEnvironment()
    }
}
