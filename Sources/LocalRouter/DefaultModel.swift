import Foundation
import UserNotifications

/// The default model, startup behaviour, and the "make this your default?"
/// suggestion when you keep loading another model instead.
extension ModelStore {
    /// Loading the same other model this many times suggests it as the default.
    static let suggestAfterLoads = 3

    var defaultModel: String? {
        get { defaultModelName }
        set {
            defaultModelName = newValue
            UserDefaults.standard.set(newValue, forKey: SettingsKey.defaultModel)
            switchCounts = [:]
            UserDefaults.standard.set(switchCounts, forKey: SettingsKey.switchCounts)
        }
    }

    func isDefault(_ model: String) -> Bool { defaultModel == model }

    /// Called after every load started from Local Router.
    func noteManualLoad(_ model: String) {
        guard let current = defaultModel, current != model else {
            switchCounts = [:]
            UserDefaults.standard.set(switchCounts, forKey: SettingsKey.switchCounts)
            return
        }
        let count = (switchCounts[model] ?? 0) + 1
        switchCounts[model] = count
        UserDefaults.standard.set(switchCounts, forKey: SettingsKey.switchCounts)

        var asked = Set(UserDefaults.standard.stringArray(forKey: SettingsKey.suggestedDefaults) ?? [])
        guard count >= Self.suggestAfterLoads, !asked.contains(model) else { return }
        asked.insert(model)
        UserDefaults.standard.set(Array(asked), forKey: SettingsKey.suggestedDefaults)
        suggestDefault(model, instead: current)
    }

    private func suggestDefault(_ model: String, instead current: String) {
        let name = aliases[model] ?? ModelIdentity.prettyName(model)
        let content = UNMutableNotificationContent()
        content.title = "Make \(name) Your Default?"
        content.body = "You've loaded it \(Self.suggestAfterLoads) times instead of \(aliases[current] ?? ModelIdentity.prettyName(current))."
        content.categoryIdentifier = NotificationIDs.suggestDefault
        content.userInfo = ["model": model]
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: "suggest-\(model)", content: content, trigger: nil))
    }

    /// The first model you download becomes the default.
    func adoptFirstDownload(_ model: String) {
        guard defaultModel == nil else { return }
        defaultModel = model
    }

    /// At launch: start Ollama and load the default model, if those are on.
    func runStartupTasks() async {
        let defaults = UserDefaults.standard
        let loadDefault = defaults.bool(forKey: SettingsKey.loadDefaultAtStartup)
        let startOllama = defaults.bool(forKey: SettingsKey.startOllamaAtStartup) || loadDefault

        // launchd forgets these on reboot, so set them before Ollama starts.
        serverSettings.applyToLaunchd()
        await refresh()
        await refreshRunningEnvironment()
        if startOllama && status == .offline {
            self.startOllama()
            for _ in 0..<30 where status == .offline || status == .checking {
                try? await Task.sleep(for: .seconds(1))
                await refresh()
            }
        }
        guard loadDefault, case .running = status, let model = defaultModel,
              installed.contains(where: { $0.name == model }),
              !loaded.contains(where: { $0.name == model }) else { return }
        await load(model, manual: false)
    }
}

enum NotificationIDs {
    static let suggestDefault = "suggestDefault"
    static let makeDefault = "makeDefault"

    static func register() {
        let make = UNNotificationAction(identifier: makeDefault, title: "Make Default")
        let category = UNNotificationCategory(identifier: suggestDefault, actions: [make], intentIdentifiers: [])
        UNUserNotificationCenter.current().setNotificationCategories([category])
    }
}
