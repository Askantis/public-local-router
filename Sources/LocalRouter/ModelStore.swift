import AppKit
import Foundation
import Observation
import UserNotifications

enum SettingsKey {
    static let ollamaURL = "ollamaURL"
    static let keepAlive = "keepAlive"
    static let aliases = "modelAliases"
    static let tags = "modelTags"
    static let onboardingDone = "onboardingDone"
    static let favorites = "favoriteModels"
    static let favoritesCollapsed = "favoritesCollapsed"
    static let loadDurations = "loadDurations"
    static let preferMLX = "preferMLX"
    static let defaultModel = "defaultModel"
    static let switchCounts = "defaultSwitchCounts"
    static let suggestedDefaults = "suggestedDefaults"
    static let loadDefaultAtStartup = "loadDefaultAtStartup"
    static let startOllamaAtStartup = "startOllamaAtStartup"
    static let menuBarMemory = "menuBarShowsMemory"
    static let menuBarContext = "menuBarShowsContext"
    static let menuBarUptime = "menuBarShowsUptime"
    static let ollamaServerSettings = "ollamaServerSettings"
    static let modelContext = "modelContextLengths"
    static let pinnedModels = "pinnedModels"
    static let schedule = "keepLoadedSchedule"
    static let sharingMode = "sharingMode"
    static let accessKeys = "accessKeys"
    static let sharePort = "sharePort"
    static let shareHTTPS = "shareHTTPS"
    static let scheduleHolding = "scheduleHoldingModel"

    /// MLX only runs on Apple silicon, so it's the default there and off on Intel Macs.
    static var preferMLXDefault: Bool {
        #if arch(arm64)
        true
        #else
        false
        #endif
    }
}

enum MemoryPressure: Equatable {
    case normal, warning, critical

    /// `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warning, 4 critical.
    static func current() -> MemoryPressure {
        var level: Int32 = 0
        var size = MemoryLayout<Int32>.size
        guard sysctlbyname("kern.memorystatus_vm_pressure_level", &level, &size, nil, 0) == 0 else { return .normal }
        switch level {
        case 4: return .critical
        case 2: return .warning
        default: return .normal
        }
    }

    var isHigh: Bool { self != .normal }

    var help: String {
        switch self {
        case .normal: "Memory pressure is normal."
        case .warning: "macOS reports memory pressure: it's compressing memory and may swap. Ejecting a model helps."
        case .critical: "macOS reports critical memory pressure. Eject a model or close apps."
        }
    }
}

/// Where Ollama keeps models, and how full that drive is.
struct StorageInfo: Equatable {
    var path: String
    var linkedFrom: String?
    var volumeName: String?
    var isExternal = false
    var total: Int64 = 0
    var free: Int64 = 0

    static func current() -> StorageInfo {
        // Ollama reads OLLAMA_MODELS if set, otherwise ~/.ollama/models (often a symlink).
        let defaultPath = NSHomeDirectory() + "/.ollama/models"
        let configured = ProcessInfo.processInfo.environment["OLLAMA_MODELS"].flatMap { $0.isEmpty ? nil : $0 }
        let url = URL(fileURLWithPath: configured ?? defaultPath)
        let resolved = url.resolvingSymlinksInPath()
        var info = StorageInfo(path: resolved.path, linkedFrom: resolved.path == url.path ? nil : url.path)
        let keys: Set<URLResourceKey> = [.volumeNameKey, .volumeTotalCapacityKey,
                                         .volumeAvailableCapacityForImportantUsageKey, .volumeIsInternalKey,
                                         .volumeIsEjectableKey, .volumeIsRootFileSystemKey]
        if let values = try? resolved.resourceValues(forKeys: keys) {
            info.volumeName = values.volumeName
            // USB and Thunderbolt SSDs don't always report themselves as external,
            // but every non-system drive is mounted under /Volumes.
            info.isExternal = values.volumeIsInternal == false || resolved.path.hasPrefix("/Volumes/")
            info.total = Int64(values.volumeTotalCapacity ?? 0)
            info.free = values.volumeAvailableCapacityForImportantUsage ?? 0
        }
        return info
    }
}

struct DownloadProgress: Equatable {
    var status = "Starting…"
    /// Across all of the model's layers.
    var completed: Int64 = 0
    var total: Int64 = 0
    /// Smoothed download speed.
    var bytesPerSecond: Double?

    var fraction: Double? { total > 0 ? min(1, Double(completed) / Double(total)) : nil }

    var secondsLeft: Double? {
        guard let bytesPerSecond, bytesPerSecond > 0, total > completed else { return nil }
        return Double(total - completed) / bytesPerSecond
    }
}

@MainActor
@Observable
final class ModelStore {
    enum Status: Equatable {
        case checking
        case running(version: String)
        case offline
    }

    enum PendingAction { case loading, ejecting, updating, deleting }

    private(set) var status: Status = .checking
    private(set) var installed: [LocalModel] = []
    private(set) var loaded: [RunningModel] = []
    private(set) var pending: [String: PendingAction] = [:]
    private(set) var storage: StorageInfo?
    /// Progress of a models-folder change (see ModelFolderChange.swift).
    var folderChange: FolderChangeState = .idle
    /// Context fill per loaded model, from llama-server (see ContextMonitor.swift).
    var contextUsage: [String: ContextUsage] = [:]
    @ObservationIgnored var modelBlobs: [String: String] = [:]
    /// What each loaded model is doing, polled every half second while busy.
    var activity: [String: ModelActivity] = [:]
    @ObservationIgnored var decodeSamples: [String: (tokens: Int, time: Date)] = [:]
    @ObservationIgnored private var activityTask: Task<Void, Never>?

    /// The model the menu bar describes: the one working, else the one with
    /// the fullest context, else the first loaded.
    var menuBarModel: RunningModel? {
        if let busy = activity.first(where: { $0.value != .idle })?.key,
           let model = loaded.first(where: { $0.name == busy }) { return model }
        let fullest = contextUsage.max { $0.value.fraction < $1.value.fraction }?.key
        return loaded.first { $0.name == fullest } ?? loaded.first
    }

    /// "13 GB | 24% | 1h", from the parts switched on in Settings.
    func menuBarText(memory: Bool, context: Bool, uptime: Bool) -> String? {
        guard let model = menuBarModel else { return nil }
        var parts: [String] = []
        if memory { parts.append(Format.compact(model.size)) }
        if context, let usage = contextUsage[model.name] {
            parts.append("\(Int((usage.fraction * 100).rounded()))%")
        }
        if uptime, let since = loadedSince[model.name] {
            parts.append(Format.uptime(since: since))
        }
        return parts.isEmpty ? nil : parts.joined(separator: " | ")
    }

    /// The busiest state across loaded models, for the menu bar dot.
    var overallActivity: ModelActivity {
        let states = activity.values
        return states.first { if case .reading = $0 { true } else { false } }
            ?? states.first { if case .writing = $0 { true } else { false } }
            ?? .idle
    }
    /// macOS's own memory pressure level (the one behind Activity Monitor's graph).
    private(set) var memoryPressure = MemoryPressure.normal
    /// Memory in use on the whole Mac, like Activity Monitor's "Memory Used".
    private(set) var systemMemoryUsed: Int64?
    var lastError: String?
    /// Your own names for models, keyed by Ollama name. Ollama's name is unchanged,
    /// so bots and scripts that use it keep working.
    private(set) var aliases: [String: String] = UserDefaults.standard.dictionary(forKey: SettingsKey.aliases) as? [String: String] ?? [:]
    /// Downloads in progress, keyed by the name passed to `ollama pull`.
    private(set) var downloads: [String: DownloadProgress] = [:]
    @ObservationIgnored private var downloadTasks: [String: Task<Void, Never>] = [:]
    /// Per model: bytes done and total for each layer, and when progress was
    /// last shown. Ollama reports every chunk; the UI updates 4 times a second.
    @ObservationIgnored private var pullLayers: [String: [String: (completed: Int64, total: Int64)]] = [:]
    @ObservationIgnored private var pullSample: [String: (time: Date, completed: Int64)] = [:]
    /// When each in-progress load started, for the progress estimate.
    private(set) var loadStarts: [String: Date] = [:]
    @ObservationIgnored private var loadDurations = UserDefaults.standard.dictionary(forKey: SettingsKey.loadDurations) as? [String: Double] ?? [:]
    /// The Log Stream window's data; Local Router's own events are noted here.
    @ObservationIgnored let logStream = LogStream()

    /// See Sharing.swift.
    var sharingModeValue = UserDefaults.standard.string(forKey: SettingsKey.sharingMode) ?? SharingMode.off.rawValue
    var accessKeysValue = ModelStore.loadAccessKeys()
    var shareState = ShareState.stopped
    @ObservationIgnored var shareProxy: ShareProxy?
    var sharePort = min(65535, max(1, UserDefaults.standard.object(forKey: SettingsKey.sharePort) as? Int ?? ModelStore.sharePortDefault)) {
        didSet {
            // Ports are 1–65535; anything else would crash when converted.
            if !(1...65535).contains(sharePort) { sharePort = oldValue; return }
            UserDefaults.standard.set(sharePort, forKey: SettingsKey.sharePort)
            if oldValue != sharePort { restartSharing() }
        }
    }
    var shareHTTPS = UserDefaults.standard.object(forKey: SettingsKey.shareHTTPS) as? Bool ?? true {
        didSet {
            UserDefaults.standard.set(shareHTTPS, forKey: SettingsKey.shareHTTPS)
            if oldValue != shareHTTPS { restartSharing() }
        }
    }

    /// See Schedule.swift.
    var schedule = KeepLoadedSchedule.load() {
        didSet { schedule.save() }
    }
    /// The model the schedule loaded and pinned, so the pin is released
    /// when the window ends, even after a restart.
    @ObservationIgnored var scheduleHolding: String? = UserDefaults.standard.string(forKey: SettingsKey.scheduleHolding) {
        didSet { UserDefaults.standard.set(scheduleHolding, forKey: SettingsKey.scheduleHolding) }
    }
    @ObservationIgnored var applyingSchedule = false
    @ObservationIgnored var scheduleAttempt: Date?

    /// Backing for `serverSettings` (see OllamaSettings.swift).
    var serverSettingsValue = OllamaSettings.load()
    /// OLLAMA_ variables the running Ollama was started with; nil if unknown.
    var runningEnvironment: [String: String]?
    var applyingSettings = false
    @ObservationIgnored let homebrewOverrides = OllamaEnvironment.homebrewOverrides()
    /// Context length per model, used whenever Local Router loads it.
    private(set) var modelContext: [String: Int] = UserDefaults.standard.dictionary(forKey: SettingsKey.modelContext) as? [String: Int] ?? [:]

    func setContextLength(_ length: Int?, for model: String) {
        modelContext[model] = length
        UserDefaults.standard.set(modelContext, forKey: SettingsKey.modelContext)
    }

    /// Backing for `defaultModel` (see DefaultModel.swift).
    var defaultModelName: String? = UserDefaults.standard.string(forKey: SettingsKey.defaultModel)
    @ObservationIgnored var switchCounts = UserDefaults.standard.dictionary(forKey: SettingsKey.switchCounts) as? [String: Int] ?? [:]
    /// When each loaded model was first seen loaded, for the uptime display.
    private(set) var loadedSince: [String: Date] = [:]
    /// Lets the notification delegate reach the store.
    @ObservationIgnored nonisolated(unsafe) static weak var current: ModelStore?
    private(set) var favorites: Set<String> = Set(UserDefaults.standard.stringArray(forKey: SettingsKey.favorites) ?? [])
    private(set) var tags: [String: [String]] = UserDefaults.standard.dictionary(forKey: SettingsKey.tags) as? [String: [String]] ?? [:]

    let totalMemory = Int64(ProcessInfo.processInfo.physicalMemory)

    @ObservationIgnored private var pollTask: Task<Void, Never>?

    init() {
        UserDefaults.standard.register(defaults: [
            SettingsKey.ollamaURL: "http://localhost:11434",
            SettingsKey.keepAlive: KeepAlive.fiveMinutes.rawValue,
            SettingsKey.preferMLX: SettingsKey.preferMLXDefault,
            SettingsKey.menuBarContext: true,
        ])
        Self.current = self
        Task { await runStartupTasks() }
        restartSharing()
        // Faster loop for context and activity: every 0.5 s while a model is
        // busy, every 1.5 s otherwise.
        activityTask = Task { [weak self] in
            while !Task.isCancelled {
                await self?.updateContextUsage()
                let busy = self?.overallActivity != .idle
                try? await Task.sleep(for: .milliseconds(busy ? 500 : 1500))
            }
        }
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                await self?.refresh()
                try? await Task.sleep(for: .seconds(3))
            }
        }
    }

    // MARK: - Derived state

    /// Installed models that aren't loaded, so each model shows up once.
    /// Installed models that aren't loaded, so each model shows up once.
    /// A model being loaded moves up into the memory card, so it's left out here.
    var notLoaded: [LocalModel] {
        let loadedNames = Set(loaded.map(\.name))
        return installed.filter { !loadedNames.contains($0.name) && pending[$0.name] != .loading }
    }

    var loadingModels: [LocalModel] {
        let loadedNames = Set(loaded.map(\.name))
        return installed.filter { pending[$0.name] == .loading && !loadedNames.contains($0.name) }
    }

    /// Ollama doesn't report load progress, so this estimates it: the last time
    /// this model took to load, or its size at a typical SSD read speed.
    func loadProgress(_ model: LocalModel, at now: Date) -> Double {
        guard let start = loadStarts[model.name] else { return 0 }
        let expected = loadDurations[model.name] ?? max(2, Double(model.size) / 600_000_000)
        let ratio = now.timeIntervalSince(start) / expected
        // Linear up to 90%, then creeps towards 99% if it takes longer than expected.
        return ratio < 0.9 ? ratio : min(0.99, 0.9 + 0.09 * (1 - exp(-(ratio - 0.9) * 3)))
    }

    /// Favorites that aren't loaded. A loaded favorite shows under Loaded,
    /// and comes back here when it's ejected.
    var favoriteModels: [LocalModel] { notLoaded.filter { favorites.contains($0.name) } }

    /// Everything else on this Mac.
    var otherModels: [LocalModel] { notLoaded.filter { !favorites.contains($0.name) } }

    func toggleFavorite(_ model: String) {
        if favorites.contains(model) {
            favorites.remove(model)
        } else {
            favorites.insert(model)
            // Show the section so the model visibly lands there.
            UserDefaults.standard.set(false, forKey: SettingsKey.favoritesCollapsed)
        }
        UserDefaults.standard.set(Array(favorites), forKey: SettingsKey.favorites)
    }

    var usedMemory: Int64 { loaded.reduce(0) { $0 + $1.size } }

    /// Models can share layers, so this slightly overstates what's on disk.
    var modelsDiskUsage: Int64 { installed.reduce(0) { $0 + $1.size } }

    func tags(for model: String) -> [ModelTag] {
        let raw = Set(tags[model] ?? [])
        return ModelTag.allCases.filter { raw.contains($0.rawValue) }
    }

    func capabilities(for model: String) -> [Capability] {
        Capability.from(installed.first { $0.name == model }?.capabilities)
    }

    /// `qwen3.5:9b` is installed, or it's the default build and `qwen3.5:latest` is.
    func isInstalled(_ pullName: String, isDefault: Bool) -> Bool {
        let names = Set(installed.map(\.name))
        if names.contains(pullName) { return true }
        guard isDefault, let base = pullName.split(separator: ":").first else { return false }
        return names.contains("\(base):latest") || names.contains(String(base))
    }

    // MARK: - Downloads

    func download(_ model: String) {
        guard downloads[model] == nil else { return }
        downloads[model] = DownloadProgress()
        let client = self.client
        // The store lives as long as the app, so capturing it strongly is fine.
        downloadTasks[model] = Task {
            do {
                try await client.pull(model) { update in
                    await self.apply(update, to: model)
                }
                finishDownload(model, error: nil)
            } catch {
                let cancelled = error is CancellationError || (error as? URLError)?.code == .cancelled
                finishDownload(model, error: cancelled ? nil : error)
            }
        }
    }

    func cancelDownload(_ model: String) {
        downloadTasks[model]?.cancel()
    }

    private func apply(_ update: PullUpdate, to model: String) {
        guard var progress = downloads[model] else { return }
        if let digest = update.digest, let total = update.total, total > 0 {
            pullLayers[model, default: [:]][digest] = (update.completed ?? 0, total)
        }
        let layers = pullLayers[model]?.values ?? [:].values
        let completed = layers.reduce(0) { $0 + $1.completed }
        let total = layers.reduce(0) { $0 + $1.total }
        let status = update.status.map { $0.prefix(1).uppercased() + $0.dropFirst() } ?? progress.status
        // "Pulling sha256-…" per layer is noise; the bar says it.
        let shownStatus = status.hasPrefix("Pulling ") && status.count > 20 ? "Downloading" : status

        let now = Date.now
        let last = pullSample[model]
        let statusChanged = shownStatus != progress.status
        guard statusChanged || last == nil || now.timeIntervalSince(last!.time) >= 0.25 else { return }

        if let last, now.timeIntervalSince(last.time) > 0, completed >= last.completed {
            let instant = Double(completed - last.completed) / now.timeIntervalSince(last.time)
            // Smooth it, so the time left doesn't jump around.
            progress.bytesPerSecond = progress.bytesPerSecond.map { $0 * 0.8 + instant * 0.2 } ?? instant
        }
        pullSample[model] = (now, completed)
        progress.status = shownStatus
        progress.completed = max(progress.completed, completed)
        progress.total = max(progress.total, total)
        downloads[model] = progress
    }

    private func finishDownload(_ model: String, error: Error?) {
        let wasCancelled = downloadTasks[model]?.isCancelled ?? false
        downloads[model] = nil
        modelBlobs[model] = nil
        pullLayers[model] = nil
        pullSample[model] = nil
        downloadTasks[model] = nil
        if let error {
            lastError = "\(model): \(error.localizedDescription)"
        } else if !wasCancelled {
            notify(title: "Download Finished", body: "\(ModelIdentity.prettyName(model)) is ready to load.")
            adoptFirstDownload(model)
        }
        Task { await refresh() }
    }

    private func notify(title: String, body: String) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        let request = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    // MARK: - Names and tags

    func rename(_ model: String, to alias: String) {
        let trimmed = alias.trimmingCharacters(in: .whitespacesAndNewlines)
        aliases[model] = trimmed.isEmpty ? nil : trimmed
        UserDefaults.standard.set(aliases, forKey: SettingsKey.aliases)
    }

    func toggle(_ tag: ModelTag, for model: String) {
        var current = Set(tags[model] ?? [])
        if current.contains(tag.rawValue) { current.remove(tag.rawValue) } else { current.insert(tag.rawValue) }
        tags[model] = current.isEmpty ? nil : Array(current)
        UserDefaults.standard.set(tags, forKey: SettingsKey.tags)
    }

    // MARK: - Actions

    func refresh() async {
        let client = self.client
        do {
            let version = try await client.version()
            async let installedModels = client.installedModels()
            async let loadedModels = client.loadedModels()
            installed = try await installedModels.sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
            loaded = try await loadedModels.sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
            status = .running(version: version)
            let now = Date.now
            var since = loadedSince.filter { name, _ in loaded.contains { $0.name == name } }
            for model in loaded where since[model.name] == nil { since[model.name] = now }
            if since != loadedSince { loadedSince = since }
            systemMemoryUsed = Self.readSystemMemoryUsed()
            let pressure = MemoryPressure.current()
            if pressure != memoryPressure {
                memoryPressure = pressure
                logStream.note("Memory pressure: \(pressure)", tone: pressure.isHigh ? .warning : .dim)
            }
            let latest = StorageInfo.current()
            if storage != latest { storage = latest }
            if !pinned.isEmpty { Task { await maintainPins() } }
            if schedule.enabled || scheduleHolding != nil { Task { await applySchedule() } }
        } catch {
            status = .offline
            installed = []
            loaded = []
            if schedule.isActive(at: .now) { Task { await applySchedule() } }
        }
    }

    /// `manual` is false for the startup load, so it doesn't count towards
    /// the "make this your default?" suggestion.
    func load(_ model: String, manual: Bool = true) async {
        logStream.note("Loading \(ModelIdentity.prettyName(model))\(modelContext[model].map { " with \(Format.tokens($0)) context" } ?? "")", tone: .busy)
        loadStarts[model] = .now
        let context = modelContext[model]
        let embedding = isEmbeddingOnly(model)
        await perform(.loading, on: model) { try await $0.load(model, keepAlive: self.keepAlive, contextLength: context, embedding: embedding) }
        if let start = loadStarts.removeValue(forKey: model), loaded.contains(where: { $0.name == model }) {
            loadDurations[model] = Date.now.timeIntervalSince(start)
            logStream.note("Loaded \(ModelIdentity.prettyName(model)) in \(String(format: "%.1f", Date.now.timeIntervalSince(start))) s", tone: .good)
            UserDefaults.standard.set(loadDurations, forKey: SettingsKey.loadDurations)
            if manual { noteManualLoad(model) }
        }
    }

    func eject(_ model: String) async {
        logStream.note("Ejecting \(ModelIdentity.prettyName(model))")
        if pinned.remove(model) != nil {
            UserDefaults.standard.set(Array(pinned), forKey: SettingsKey.pinnedModels)
        }
        let embedding = isEmbeddingOnly(model)
        await perform(.ejecting, on: model) { try await $0.eject(model, embedding: embedding) }
    }

    /// Ejects the model if it's loaded, then removes it from disk.
    func delete(_ model: String) async {
        let wasLoaded = loaded.contains { $0.name == model }
        let embedding = isEmbeddingOnly(model)
        await perform(.deleting, on: model) { client in
            if wasLoaded { try await client.eject(model, embedding: embedding) }
            try await client.delete(model)
        }
        if !installed.contains(where: { $0.name == model }) {
            rename(model, to: "")
            modelBlobs[model] = nil
            if favorites.contains(model) { toggleFavorite(model) }
            if defaultModel == model { defaultModel = nil }
            tags[model] = nil
            UserDefaults.standard.set(tags, forKey: SettingsKey.tags)
        }
    }

    /// Pinned models (Keep Alive). Ollama takes the unload timer from the
    /// latest request, so any app's next request would undo a pin; Local
    /// Router keeps this list and re-pins after such requests.
    private(set) var pinned: Set<String> = Set(UserDefaults.standard.stringArray(forKey: SettingsKey.pinnedModels) ?? [])
    @ObservationIgnored private var repinning: Set<String> = []
    /// After a failed re-pin, wait before trying again.
    @ObservationIgnored private var repinFailedAt: [String: Date] = [:]

    func isKeptAlive(_ model: RunningModel) -> Bool { pinned.contains(model.name) }

    /// Keep Alive on: stays loaded until ejected. Off: back to the idle time from Settings.
    func setKeepAlive(_ on: Bool, for model: String) async {
        if on { pinned.insert(model) } else { pinned.remove(model) }
        logStream.note("\(on ? "Pinned" : "Unpinned") \(ModelIdentity.prettyName(model))")
        UserDefaults.standard.set(Array(pinned), forKey: SettingsKey.pinnedModels)
        let fallback = keepAlive == .untilEjected ? KeepAlive.fiveMinutes : keepAlive
        await sendKeepAlive(on ? .untilEjected : fallback, to: model)
        await refresh()
    }

    /// Re-pins pinned models whose timer another app's request reset.
    func maintainPins() async {
        for model in loaded where pinned.contains(model.name) && !model.staysLoaded && !repinning.contains(model.name) {
            if let failed = repinFailedAt[model.name], Date.now.timeIntervalSince(failed) < 60 { continue }
            repinning.insert(model.name)
            logStream.note("Re-pinning \(ModelIdentity.prettyName(model.name)): another app's request reset its unload timer", tone: .dim)
            if !(await sendKeepAlive(.untilEjected, to: model.name)) { repinFailedAt[model.name] = .now }
            repinning.remove(model.name)
        }
    }

    /// Always with the loaded context size: a different `num_ctx` would make
    /// Ollama reload the model and lose its cache.
    @discardableResult
    private func sendKeepAlive(_ keepAlive: KeepAlive, to model: String) async -> Bool {
        let context = loaded.first { $0.name == model }?.contextLength ?? modelContext[model]
        do {
            try await client.load(model, keepAlive: keepAlive, contextLength: context, embedding: isEmbeddingOnly(model))
            return true
        } catch {
            lastError = "\(model): \(error.localizedDescription)"
            return false
        }
    }

    /// Embedding models can't generate, so they're loaded through /api/embed.
    func isEmbeddingOnly(_ model: String) -> Bool {
        guard let capabilities = installed.first(where: { $0.name == model })?.capabilities else { return false }
        return !capabilities.contains("completion") && capabilities.contains("embedding")
    }

    func ejectAll() async {
        await withTaskGroup(of: Void.self) { group in
            for model in loaded.map(\.name) {
                group.addTask { await self.eject(model) }
            }
        }
    }

    /// Opens Ollama.app if installed, otherwise starts the Homebrew service
    /// (which keeps its configured environment), otherwise runs `ollama serve`.
    func startOllama() {
        status = .checking
        if let app = NSWorkspace.shared.urlForApplication(withBundleIdentifier: "com.electron.ollama") {
            NSWorkspace.shared.openApplication(at: app, configuration: .init())
        } else if let brew = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"].first(where: FileManager.default.isExecutableFile),
                  FileManager.default.fileExists(atPath: (brew as NSString).deletingLastPathComponent + "/ollama") {
            run(brew, ["services", "start", "ollama"])
        } else if let ollama = ["/opt/homebrew/bin/ollama", "/usr/local/bin/ollama"].first(where: FileManager.default.isExecutableFile) {
            run(ollama, ["serve"])
        } else {
            status = .offline
            lastError = "Couldn't find Ollama. Install it from ollama.com."
            return
        }
        Task {
            try? await Task.sleep(for: .seconds(2))
            await refresh()
        }
    }

    // MARK: - Helpers

    /// App memory + wired + compressed, which is how Activity Monitor counts "Memory Used".
    private static func readSystemMemoryUsed() -> Int64? {
        var stats = vm_statistics64()
        var count = mach_msg_type_number_t(MemoryLayout<vm_statistics64_data_t>.stride / MemoryLayout<integer_t>.stride)
        let result = withUnsafeMutablePointer(to: &stats) {
            $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                host_statistics64(mach_host_self(), HOST_VM_INFO64, $0, &count)
            }
        }
        guard result == KERN_SUCCESS else { return nil }
        let page = Int64(getpagesize())
        let appMemory = Int64(stats.internal_page_count) - Int64(stats.purgeable_count)
        return (appMemory + Int64(stats.wire_count) + Int64(stats.compressor_page_count)) * page
    }

    var client: OllamaClient {
        let string = UserDefaults.standard.string(forKey: SettingsKey.ollamaURL) ?? ""
        return OllamaClient(baseURL: URL(string: string) ?? URL(string: "http://localhost:11434")!)
    }

    private var keepAlive: KeepAlive {
        KeepAlive(rawValue: UserDefaults.standard.string(forKey: SettingsKey.keepAlive) ?? "") ?? .fiveMinutes
    }

    private func perform(_ action: PendingAction, on model: String, _ work: (OllamaClient) async throws -> Void) async {
        pending[model] = action
        // Another action may have started on this model meanwhile; leave its flag.
        defer { if pending[model] == action { pending[model] = nil } }
        do {
            try await work(client)
            lastError = nil
        } catch {
            lastError = "\(model): \(error.localizedDescription)"
        }
        await refresh()
    }

    private func run(_ executable: String, _ arguments: [String]) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
        } catch {
            lastError = "Couldn't start Ollama: \(error.localizedDescription)"
        }
    }
}
