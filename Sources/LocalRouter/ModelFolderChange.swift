import AppKit
import Foundation

/// How Ollama is running, so it can be stopped and started again the same way.
enum OllamaRunner {
    case app(NSRunningApplication)
    case brewService(brew: String)
    case serve(pid: Int32, executable: String)
    case notRunning

    static func detect() -> OllamaRunner {
        if let app = NSRunningApplication.runningApplications(withBundleIdentifier: "com.electron.ollama").first {
            return .app(app)
        }
        if let brew = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"].first(where: FileManager.default.isExecutableFile),
           shell(brew, ["services", "list"]).split(separator: "\n").contains(where: { $0.hasPrefix("ollama ") && $0.contains("started") }) {
            return .brewService(brew: brew)
        }
        let serve = shell("/usr/bin/pgrep", ["-f", "ollama serve"]).split(separator: "\n").compactMap { Int32($0) }.first
        if let serve, let executable = ["/opt/homebrew/bin/ollama", "/usr/local/bin/ollama"].first(where: FileManager.default.isExecutableFile) {
            return .serve(pid: serve, executable: executable)
        }
        return .notRunning
    }

    /// Where `ollama serve` started by Local Router writes its log.
    static let serveLogPath = NSHomeDirectory() + "/Library/Logs/Local Router/ollama.log"

    func stop() {
        switch self {
        case .app(let app): app.terminate()
        case .brewService(let brew): _ = OllamaRunner.shell(brew, ["services", "stop", "ollama"])
        case .serve(let pid, _): kill(pid, SIGTERM)
        case .notRunning: break
        }
    }

    /// `environment` is used for `ollama serve`, which Local Router starts itself;
    /// the app and the Homebrew service read launchd's environment instead.
    func start(environment: [String: String?] = [:]) {
        switch self {
        case .app(let app):
            if let url = app.bundleURL { NSWorkspace.shared.openApplication(at: url, configuration: .init()) }
        case .brewService(let brew):
            _ = OllamaRunner.shell(brew, ["services", "start", "ollama"])
        case .serve(_, let executable):
            let process = Process()
            process.executableURL = URL(fileURLWithPath: executable)
            process.arguments = ["serve"]
            var env = ProcessInfo.processInfo.environment
            for (key, value) in environment { env[key] = value }
            process.environment = env
            // Log to a file, so the Log Stream window can follow it.
            let log = URL(fileURLWithPath: OllamaRunner.serveLogPath)
            try? FileManager.default.createDirectory(at: log.deletingLastPathComponent(), withIntermediateDirectories: true)
            if !FileManager.default.fileExists(atPath: log.path) { FileManager.default.createFile(atPath: log.path, contents: nil) }
            let output = (try? FileHandle(forWritingTo: log)) ?? FileHandle.nullDevice
            _ = try? output.seekToEnd()
            process.standardOutput = output
            process.standardError = output
            try? process.run()
        case .notRunning:
            break
        }
    }

    @discardableResult
    static func shell(_ path: String, _ arguments: [String]) -> String {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: path)
        process.arguments = arguments
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        guard (try? process.run()) != nil else { return "" }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return String(decoding: data, as: UTF8.self)
    }
}

extension ModelStore {
    enum FolderChangeState: Equatable {
        case idle
        case working(step: String, fraction: Double?)
        case finished(String)
        case failed(String)
    }

    /// What changing to `folder` would involve, for the confirmation prompt.
    struct FolderChangePlan: Identifiable {
        let folder: URL
        let modelCount: Int
        let bytes: Int64
        let folderHasModels: Bool
        let enoughSpace: Bool
        var id: String { folder.path }
    }

    func plan(for folder: URL) -> FolderChangePlan {
        let bytes = modelsDiskUsage
        let free = (try? folder.resourceValues(forKeys: [.volumeAvailableCapacityForImportantUsageKey]).volumeAvailableCapacityForImportantUsage) ?? 0
        let current = storage.map { URL(fileURLWithPath: $0.path) }
        let sameDrive = current.map { ModelFolderMover.sameVolume($0, folder) } ?? false
        return FolderChangePlan(folder: folder, modelCount: installed.count, bytes: bytes,
                                folderHasModels: ModelFolderMover.containsModels(folder),
                                enoughSpace: sameDrive || free > bytes + 1_000_000_000)
    }

    /// Points Ollama at `folder`, moving the current models there if asked.
    /// Ollama is stopped for the move and started again the way it was running.
    func changeModelsFolder(to folder: URL, moveModels: Bool) async {
        if let reason = folderChangeBlocker() {
            folderChange = .failed(reason)
            return
        }
        guard let current = storage.map({ URL(fileURLWithPath: $0.path) }) else { return }
        // A folder inside the current one (or the link) would be moved into
        // itself, or renamed away with it.
        let target = folder.standardizedFileURL.resolvingSymlinksInPath().path
        for inside in [current.standardizedFileURL.resolvingSymlinksInPath().path, ModelFolderMover.linkPath.standardizedFileURL.path] {
            if target == inside || target.hasPrefix(inside + "/") {
                folderChange = .failed("Choose a folder outside the current models folder.")
                return
            }
        }
        guard downloads.isEmpty else {
            folderChange = .failed("Wait for downloads to finish, then try again.")
            return
        }
        let runner = OllamaRunner.detect()

        folderChange = .working(step: "Stopping Ollama…", fraction: nil)
        runner.stop()
        await waitForOllama(running: false)
        // Never move files Ollama still has open.
        guard status == .offline else {
            runner.start()
            folderChange = .failed("Couldn't stop Ollama, so nothing was moved. Quit Ollama and try again.")
            return
        }

        do {
            if moveModels && current.standardizedFileURL != folder.standardizedFileURL {
                folderChange = .working(step: "Moving models…", fraction: 0)
                try await Task.detached(priority: .userInitiated) {
                    try ModelFolderMover.transfer(from: current, to: folder) { fraction in
                        Task { @MainActor in self.folderChange = .working(step: "Moving models…", fraction: fraction) }
                    }
                }.value
            }
            folderChange = .working(step: "Pointing Ollama to the new folder…", fraction: nil)
            try ModelFolderMover.repoint(to: folder)
        } catch {
            runner.start()
            folderChange = .failed("\(error.localizedDescription) Ollama was restarted with the old folder.")
            return
        }

        folderChange = .working(step: "Starting Ollama…", fraction: nil)
        runner.start()
        await waitForOllama(running: true)
        await refresh()
        folderChange = .finished(moveModels
            ? "Moved \(installed.count) models to \(folder.path)."
            : "Ollama now uses \(folder.path). It has \(installed.count) models.")
    }

    /// A symlink at ~/.ollama/models only works if nothing overrides it.
    private func folderChangeBlocker() -> String? {
        let fromLaunchd = OllamaRunner.shell("/bin/launchctl", ["getenv", "OLLAMA_MODELS"]).trimmingCharacters(in: .whitespacesAndNewlines)
        let fromEnvironment = ProcessInfo.processInfo.environment["OLLAMA_MODELS"] ?? ""
        if let set = [fromLaunchd, fromEnvironment].first(where: { !$0.isEmpty }) {
            return "OLLAMA_MODELS is set to \(set), which overrides the models folder. Remove it first (launchctl unsetenv OLLAMA_MODELS)."
        }
        return nil
    }

    private func waitForOllama(running: Bool) async {
        for _ in 0..<40 {
            await refresh()
            if (status != .offline) == running, status != .checking { return }
            try? await Task.sleep(for: .milliseconds(500))
        }
    }
}
