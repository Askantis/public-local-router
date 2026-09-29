import AppKit
import Foundation
import Observation

/// One line in the Log Stream window.
struct LogLine: Identifiable, Equatable {
    enum Kind: String, CaseIterable, Identifiable {
        case request, model, connection, app, raw
        var id: String { rawValue }

        var label: String {
            switch self {
            case .request: "Requests"
            case .model: "Model"
            case .connection: "Connections"
            case .app: "Local Router"
            case .raw: "Raw Log"
            }
        }
    }

    let id = UUID()
    var date: Date
    let kind: Kind
    /// Who it's about: "ollama", "hermes", "local router", a network address…
    let source: String
    let text: String
    var isPolling = false
    var tone: Tone = .normal

    enum Tone { case normal, dim, good, busy, warning, error }
}

/// Tails Ollama's log, turns it into readable events, and watches which apps
/// are connected to Ollama, so requests can be traced to where they came from.
@MainActor
@Observable
final class LogStream {
    private(set) var lines: [LogLine] = []
    private(set) var logPath: String?
    private(set) var clients: [Client] = []

    struct Client: Hashable {
        let pid: Int32
        let name: String
        let isNetwork: Bool
    }

    @ObservationIgnored private var handle: FileHandle?
    /// The open file's inode, to notice when Ollama rotates or replaces its log.
    @ObservationIgnored private var openFile: UInt64?
    @ObservationIgnored private var offset: UInt64 = 0
    @ObservationIgnored private var partial = ""
    @ObservationIgnored private var task: Task<Void, Never>?
    @ObservationIgnored private var recentClients: [(client: Client, seen: Date)] = []
    /// Only llama.cpp's lines have no timestamp; while reading older lines they
    /// get the time of the last request line before them.
    @ObservationIgnored private var lastLoggedTime: Date?
    @ObservationIgnored private var readingBacklog = false
    private let maxLines = 3000

    func start() {
        guard task == nil else { return }
        task = Task { [weak self] in
            var tick = 0
            while !Task.isCancelled {
                if tick % 6 == 0 { self?.reopenIfReplaced() }
                self?.readNewLines()
                if tick % 2 == 0 { await self?.updateClients() }
                tick += 1
                try? await Task.sleep(for: .milliseconds(500))
            }
        }
    }

    func stop() {
        task?.cancel()
        task = nil
    }

    func clear() { lines.removeAll() }

    /// Adds a line from elsewhere in the app (e.g. Sharing).
    func appendEvent(_ line: LogLine) { append(line) }

    /// Events from Local Router itself (loads, pins, schedule…).
    func note(_ text: String, tone: LogLine.Tone = .normal) {
        append(LogLine(date: .now, kind: .app, source: "local router", text: text, tone: tone))
    }

    // MARK: - Log file

    /// Where the running Ollama writes: its stdout file if it has one, else the
    /// Homebrew service log, else the Ollama app's log.
    static func findLogPath() -> String? {
        if let pid = OllamaRunner.shell("/usr/bin/pgrep", ["-x", "ollama"]).split(separator: "\n").first {
            let files = OllamaRunner.shell("/usr/sbin/lsof", ["-a", "-p", String(pid), "-d", "1,2", "-Fn"])
            for line in files.split(separator: "\n") where line.hasPrefix("n/") {
                let path = String(line.dropFirst())
                if FileManager.default.isReadableFile(atPath: path), !path.hasPrefix("/dev/") { return path }
            }
        }
        let candidates = [
            "/opt/homebrew/var/log/ollama.log", "/usr/local/var/log/ollama.log",
            NSHomeDirectory() + "/.ollama/logs/server.log",
            OllamaRunner.serveLogPath,
        ]
        return candidates
            .filter { FileManager.default.fileExists(atPath: $0) }
            .max { modified($0) < modified($1) }
    }

    private static func modified(_ path: String) -> Date {
        (try? FileManager.default.attributesOfItem(atPath: path)[.modificationDate] as? Date) ?? .distantPast
    }

    /// Ollama renames server.log to server-1.log when it starts, and a
    /// different install writes elsewhere; follow the current file.
    private func reopenIfReplaced() {
        guard handle != nil else { return }
        let current = Self.findLogPath()
        if current != logPath || (current.flatMap(Self.inode) != openFile) {
            try? handle?.close()
            handle = nil
            openFile = nil
        }
    }

    private static func inode(_ path: String) -> UInt64? {
        (try? FileManager.default.attributesOfItem(atPath: path)[.systemFileNumber] as? NSNumber)?.uint64Value
    }

    private func readNewLines() {
        if handle == nil {
            guard let path = Self.findLogPath(), let file = FileHandle(forReadingAtPath: path) else {
                if logPath != nil || lines.isEmpty {
                    logPath = nil
                    if lines.isEmpty { note("Waiting for Ollama's log. Start Ollama, or it may be running in a terminal.", tone: .dim) }
                }
                return
            }
            handle = file
            openFile = Self.inode(path)
            logPath = path
            // Start with the last part of the file, not all of it.
            let size = (try? file.seekToEnd()) ?? 0
            offset = size > 96_000 ? size - 96_000 : 0
            partial = ""
            readingBacklog = true
            note("Reading \(path)", tone: .dim)
        }
        guard let handle else { return }
        let size = (try? handle.seekToEnd()) ?? 0
        if size < offset { offset = 0 } // rotated or truncated
        guard size > offset else { return }
        try? handle.seek(toOffset: offset)
        guard let data = try? handle.read(upToCount: Int(min(size - offset, 2_000_000))) else { return }
        offset += UInt64(data.count)
        let text = partial + String(decoding: data, as: UTF8.self)
        var rows = text.components(separatedBy: "\n")
        partial = rows.removeLast()
        for row in rows where !row.isEmpty {
            var line = LogParser.parse(row, clients: activeClients)
            if let logged = LogParser.timestamp(row) {
                lastLoggedTime = logged
                line.date = logged
            } else if readingBacklog, let lastLoggedTime {
                line.date = lastLoggedTime
            }
            append(line)
        }
        readingBacklog = false
    }

    /// Progress lines ("Generating · 689 tokens", "Reading prompt 40%") come
    /// many times a request; they update one line instead of adding more.
    private func append(_ line: LogLine) {
        // Look back over the last few visible lines (status checks and raw
        // lines are usually hidden) for the same kind of progress line.
        if let prefix = LogParser.progressPrefix(line.text) {
            var visibleSeen = 0
            for index in lines.indices.reversed() {
                let candidate = lines[index]
                if candidate.isPolling || candidate.kind == .raw { continue }
                if candidate.kind == line.kind && candidate.text.hasPrefix(prefix) {
                    lines[index] = line
                    return
                }
                visibleSeen += 1
                if visibleSeen >= 3 || LogParser.progressPrefix(candidate.text) == nil { break }
            }
        }
        lines.append(line)
        if lines.count > maxLines { lines.removeFirst(lines.count - maxLines) }
    }

    // MARK: - Connections

    /// Apps seen talking to Ollama in the last few seconds.
    private var activeClients: [Client] {
        let cutoff = Date.now.addingTimeInterval(-10)
        return recentClients.filter { $0.seen > cutoff }.map(\.client)
    }

    private func updateClients() async {
        let port = (URL(string: UserDefaults.standard.string(forKey: SettingsKey.ollamaURL) ?? "")?.port) ?? 11434
        let found = await Task.detached { Self.connectedClients(port: port) }.value
        let before = Set(clients)
        let now = Set(found)
        for client in now.subtracting(before) {
            append(LogLine(date: .now, kind: .connection, source: client.name.lowercased(),
                           text: "\(client.name) connected\(client.pid > 0 ? " (pid \(client.pid))" : "")", tone: .good))
        }
        for client in before.subtracting(now) {
            append(LogLine(date: .now, kind: .connection, source: client.name.lowercased(),
                           text: "\(client.name) disconnected", tone: .dim))
        }
        clients = found
        let stamp = Date.now
        recentClients.removeAll { $0.seen < stamp.addingTimeInterval(-10) }
        for client in found { recentClients.append((client, stamp)) }
    }

    /// Processes with a TCP connection to Ollama's port, other than Ollama and
    /// Local Router; plus remote addresses when Ollama is exposed to the network.
    nonisolated static func connectedClients(port: Int) -> [Client] {
        let output = OllamaRunner.shell("/usr/sbin/lsof", ["-nP", "-iTCP:\(port)", "-sTCP:ESTABLISHED", "-F", "pcn"])
        let me = ProcessInfo.processInfo.processIdentifier
        var result: Set<Client> = []
        var pid: Int32 = 0
        var command = ""
        for line in output.split(separator: "\n") {
            switch line.first {
            case "p": pid = Int32(line.dropFirst()) ?? 0
            case "c": command = String(line.dropFirst())
            case "n":
                let connection = String(line.dropFirst())
                let sides = connection.components(separatedBy: "->")
                guard sides.count == 2 else { continue }
                if command == "ollama" {
                    // Ollama's side: a remote peer means a device on the network.
                    let peer = sides[1]
                    if !peer.hasPrefix("127.0.0.1") && !peer.hasPrefix("[::1]") {
                        let host = peer.components(separatedBy: ":").dropLast().joined(separator: ":")
                        result.insert(Client(pid: 0, name: "Network \(host)", isNetwork: true))
                    }
                } else if pid != me, sides[1].hasSuffix(":\(port)") {
                    result.insert(Client(pid: pid, name: appName(pid: pid, command: command), isNetwork: false))
                }
            default: break
            }
        }
        return result.sorted { $0.name < $1.name }
    }

    /// "Hermes" for Hermes Agent, the .app name for apps, else the process name.
    nonisolated static func appName(pid: Int32, command: String) -> String {
        let full = OllamaRunner.shell("/bin/ps", ["-p", String(pid), "-o", "command="])
        if full.contains(".hermes/") || full.contains("hermes_cli") { return "Hermes" }
        if let range = full.range(of: #"/([^/]+)\.app/"#, options: .regularExpression) {
            return String(full[range].dropFirst().dropLast(5))
        }
        return command
    }
}

/// Turns Ollama and llama.cpp log lines into readable events.
enum LogParser {
    static func parse(_ raw: String, clients: [LogStream.Client]) -> LogLine {
        let text = raw.trimmingCharacters(in: .whitespaces)

        // [GIN] 2026/09/29 - 02:55:49 | 200 | 1m2s | 127.0.0.1 | POST "/api/chat"
        if let match = text.firstMatch(of: /\[GIN\] (\S+ - \S+) \| (\d+) \|\s*(\S+) \|\s*(\S+) \| (\w+)\s+"([^"]+)"/) {
            let status = Int(match.2) ?? 0
            let method = String(match.5), path = String(match.6), address = String(match.4)
            let polling = method == "GET" && ["/api/version", "/api/ps", "/api/tags"].contains(path)
                || method == "POST" && path == "/api/show"
            let source = requestSource(address: address, clients: clients, isPolling: polling)
            return LogLine(date: .now, kind: .request, source: source,
                           text: "\(method) \(path)  \(status)  \(duration(match.3))",
                           isPolling: polling, tone: status >= 400 ? .error : (polling ? .dim : .normal))
        }
        if let match = text.firstMatch(of: /new prompt, n_ctx_slot = (\d+).*task\.n_tokens = (\d+)/) {
            return LogLine(date: .now, kind: .model, source: "ollama",
                           text: "New prompt · \(Int(match.2)?.formatted() ?? "?") tokens (context \(Format.tokens(Int(match.1) ?? 0)))", tone: .busy)
        }
        if let match = text.firstMatch(of: /cached n_tokens = (\d+)/) {
            return LogLine(date: .now, kind: .model, source: "ollama",
                           text: "Reusing \(Int(match.1)?.formatted() ?? "?") cached tokens", tone: .good)
        }
        if let match = text.firstMatch(of: /prompt processing, n_tokens =\s*(\d+), progress = ([\d.]+)/) {
            let percent = Int((Double(match.2) ?? 0) * 100)
            return LogLine(date: .now, kind: .model, source: "ollama",
                           text: "Reading prompt \(percent)% · \(Int(match.1)?.formatted() ?? "?") tokens", tone: .busy)
        }
        if let match = text.firstMatch(of: /n_gen =\s*(\d+), tg =\s*([\d.]+) t\/s/) {
            return LogLine(date: .now, kind: .model, source: "ollama",
                           text: "Generating · \(match.1) tokens · \(match.2) tok/s", tone: .good)
        }
        if let match = text.firstMatch(of: /prompt eval time =\s*([\d.]+) ms \/\s*(\d+) tokens.*?([\d.]+) tokens per second/) {
            return LogLine(date: .now, kind: .model, source: "ollama",
                           text: "Prompt: \(match.2) tokens in \(seconds(match.1)) · \(match.3) tok/s")
        }
        if let match = text.firstMatch(of: /^.*\beval time =\s*([\d.]+) ms \/\s*(\d+) tokens.*?([\d.]+) tokens per second/) {
            return LogLine(date: .now, kind: .model, source: "ollama",
                           text: "Output: \(match.2) tokens in \(seconds(match.1)) · \(match.3) tok/s")
        }
        if let match = text.firstMatch(of: /total time =\s*([\d.]+) ms \/\s*(\d+) tokens/) {
            return LogLine(date: .now, kind: .model, source: "ollama",
                           text: "Done · \(match.2) tokens in \(seconds(match.1))", tone: .good)
        }
        if text.contains("stop processing") {
            let tokens = text.firstMatch(of: /n_tokens = (\d+)/).map { Int($0.1)?.formatted() ?? "" } ?? ""
            return LogLine(date: .now, kind: .model, source: "ollama", text: "Finished request · context now \(tokens) tokens")
        }
        if text.contains("all slots are idle") {
            return LogLine(date: .now, kind: .model, source: "ollama", text: "Idle", tone: .dim)
        }
        if text.localizedCaseInsensitiveContains("error") || text.contains("level=ERROR") {
            return LogLine(date: .now, kind: .raw, source: "ollama", text: text, tone: .error)
        }
        if text.contains("level=WARN") {
            return LogLine(date: .now, kind: .raw, source: "ollama", text: text, tone: .warning)
        }
        return LogLine(date: .now, kind: .raw, source: "ollama", text: text, tone: .dim)
    }

    /// Local requests are traced to the app connected at the time; network
    /// requests show their address.
    /// Local Router only makes status checks, so anything else from this Mac
    /// is another app, named when exactly one is connected.
    private static func requestSource(address: String, clients: [LogStream.Client], isPolling: Bool) -> String {
        guard address == "127.0.0.1" || address == "::1" else { return address }
        let apps = Set(clients.filter { !$0.isNetwork }.map(\.name))
        if !isPolling {
            if apps.count == 1, let app = apps.first { return app.lowercased() }
            return "local app"
        }
        return apps.isEmpty ? "local router" : "local app"
    }

    /// Go durations ("25.793031708s", "5.51ms", "24.08µs", "1m2.5s") made readable.
    static func duration(_ raw: Substring) -> String {
        let text = String(raw)
        if text.hasSuffix("µs") || text.hasSuffix("ns") { return "<1 ms" }
        if text.hasSuffix("ms"), let value = Double(text.dropLast(2)) { return "\(Int(value.rounded())) ms" }
        if let minutes = text.firstMatch(of: /^(\d+)m([\d.]+)s$/) {
            return "\(minutes.1)m \(Int(Double(minutes.2) ?? 0))s"
        }
        if text.hasSuffix("s"), let value = Double(text.dropLast()) { return String(format: "%.1f s", value) }
        return text
    }

    /// Lines that report ongoing progress, and are merged into one.
    static func progressPrefix(_ text: String) -> String? {
        ["Generating ·", "Reading prompt", "Reusing "].first { text.hasPrefix($0) }
    }

    /// The time in a GIN request line ("2026/09/29 - 02:55:49"), if any.
    static func timestamp(_ raw: String) -> Date? {
        guard let match = raw.firstMatch(of: /\[GIN\] (\d{4}\/\d{2}\/\d{2} - \d{2}:\d{2}:\d{2})/) else { return nil }
        return ginFormatter.date(from: String(match.1))
    }

    private static let ginFormatter: DateFormatter = {
        let formatter = DateFormatter()
        formatter.dateFormat = "yyyy/MM/dd - HH:mm:ss"
        formatter.locale = Locale(identifier: "en_US_POSIX")
        return formatter
    }()

    private static func seconds(_ milliseconds: Substring) -> String {
        let value = (Double(milliseconds) ?? 0) / 1000
        return value < 60 ? String(format: "%.1f s", value) : "\(Int(value) / 60)m \(Int(value) % 60)s"
    }
}
