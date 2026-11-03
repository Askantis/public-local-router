import AppKit
import Foundation
import Observation
import ServiceManagement
import SwiftUI

// Local Router's background service, localrouterd: starting it, talking to it
// over its Unix socket, and showing the apps that use it. See spec/PROTOCOL.md.

/// Paths the service uses (PROTOCOL.md 3.1, 4.1).
enum DaemonPaths {
    static var supportDir: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Local Router", isDirectory: true)
    }
    static var socket: String { supportDir.appendingPathComponent("lr.sock").path }
    static var endpoint: URL { supportDir.appendingPathComponent("endpoint.json") }
}

// MARK: - HTTP over the Unix socket

/// A minimal HTTP/1.1 client over a Unix socket: one request per connection.
/// The admin endpoints answer only here, checked by code signature, so there
/// is no token to keep (PROTOCOL.md 5.3).
enum UnixHTTP {
    struct Response: Sendable {
        let status: Int
        let body: Data
        var json: Any? { try? JSONSerialization.jsonObject(with: body) }
    }

    enum Failure: Error { case connect, io }

    static func connect(_ path: String) throws -> Int32 {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw Failure.connect }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: addr.sun_path) else { close(fd); throw Failure.connect }
        withUnsafeMutableBytes(of: &addr.sun_path) { buf in
            buf.copyBytes(from: bytes)
            buf[bytes.count] = 0
        }
        let ok = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard ok == 0 else { close(fd); throw Failure.connect }
        var noSigPipe: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
        return fd
    }

    static func head(_ method: String, _ path: String, body: Data?, keepAlive: Bool) -> Data {
        var head = "\(method) \(path) HTTP/1.1\r\nHost: lr\r\nConnection: \(keepAlive ? "keep-alive" : "close")\r\n"
        if let body {
            head += "Content-Type: application/json\r\nContent-Length: \(body.count)\r\n"
        } else if method != "GET" {
            head += "Content-Length: 0\r\n"
        }
        head += "\r\n"
        var data = Data(head.utf8)
        if let body { data.append(body) }
        return data
    }

    static func writeAll(_ fd: Int32, _ data: Data) throws {
        try data.withUnsafeBytes { raw in
            var off = 0
            while off < raw.count {
                let n = Darwin.write(fd, raw.baseAddress! + off, raw.count - off)
                if n <= 0 { throw Failure.io }
                off += n
            }
        }
    }

    /// Blocking; call from a background task.
    static func request(_ method: String, _ path: String, json: Any? = nil, socketPath: String = DaemonPaths.socket) throws -> Response {
        let fd = try connect(socketPath)
        defer { close(fd) }
        let body = try json.map { try JSONSerialization.data(withJSONObject: $0) }
        try writeAll(fd, head(method, path, body: body, keepAlive: false))
        var raw = Data()
        var buf = [UInt8](repeating: 0, count: 64 * 1024)
        while true {
            let n = read(fd, &buf, buf.count)
            if n < 0 { throw Failure.io }
            if n == 0 { break }
            raw.append(contentsOf: buf[0..<n])
        }
        return try parse(raw)
    }

    static func parse(_ raw: Data) throws -> Response {
        guard let split = raw.range(of: Data("\r\n\r\n".utf8)) else { throw Failure.io }
        let headText = String(decoding: raw[..<split.lowerBound], as: UTF8.self)
        let lines = headText.components(separatedBy: "\r\n")
        let status = lines.first.flatMap { Int($0.split(separator: " ").dropFirst().first ?? "") } ?? 0
        var body = Data(raw[split.upperBound...])
        if lines.contains(where: { $0.lowercased().hasPrefix("transfer-encoding:") && $0.lowercased().contains("chunked") }) {
            var dechunker = Dechunker()
            body = dechunker.feed(body)
        }
        return Response(status: status, body: body)
    }

    /// Undoes chunked transfer encoding, fed piece by piece.
    struct Dechunker {
        private var pending = Data()
        private var remaining = 0  // bytes left in the current chunk, plus its CRLF

        mutating func feed(_ data: Data) -> Data {
            pending.append(data)
            var out = Data()
            while true {
                if remaining > 0 {
                    let take = min(remaining, pending.count)
                    let chunk = pending.prefix(take)
                    // The last two bytes of a chunk are its CRLF.
                    let payload = remaining > 2 ? chunk.prefix(max(0, min(take, remaining - 2))) : Data()
                    out.append(payload)
                    remaining -= take
                    pending.removeFirst(take)
                    if remaining > 0 { return out }
                    continue
                }
                guard let nl = pending.range(of: Data("\r\n".utf8)) else { return out }
                let sizeText = String(decoding: pending[..<nl.lowerBound], as: UTF8.self)
                pending.removeSubrange(..<nl.upperBound)
                guard let size = Int(sizeText.split(separator: ";").first ?? "", radix: 16) else { return out }
                if size == 0 { return out }
                remaining = size + 2
            }
        }
    }

    /// Follow the server-sent event stream (PROTOCOL.md 10) until it ends.
    /// Blocking; run on its own thread.
    static func events(onEvent: (String, Data) -> Void) throws {
        let fd = try connect(DaemonPaths.socket)
        defer { close(fd) }
        try writeAll(fd, head("GET", "/lr/v1/events", body: nil, keepAlive: true))
        var header = Data()
        var dechunker = Dechunker()
        var text = ""
        var inBody = false
        var buf = [UInt8](repeating: 0, count: 16 * 1024)
        while true {
            let n = read(fd, &buf, buf.count)
            if n <= 0 { return }
            var data = Data(buf[0..<n])
            if !inBody {
                header.append(data)
                guard let split = header.range(of: Data("\r\n\r\n".utf8)) else { continue }
                inBody = true
                data = Data(header[split.upperBound...])
            }
            text += String(decoding: dechunker.feed(data), as: UTF8.self)
            while let end = text.range(of: "\n\n") {
                let block = String(text[..<end.lowerBound])
                text.removeSubrange(..<end.upperBound)
                var name = "message"
                var payload = ""
                for line in block.split(separator: "\n", omittingEmptySubsequences: false) {
                    if line.hasPrefix("event:") { name = line.dropFirst(6).trimmingCharacters(in: .whitespaces) }
                    if line.hasPrefix("data:") { payload += line.dropFirst(5).trimmingCharacters(in: .whitespaces) }
                }
                if !payload.isEmpty { onEvent(name, Data(payload.utf8)) }
            }
        }
    }
}

// MARK: - The service and the apps that use it

struct ConnectedApp: Identifiable, Hashable, Sendable {
    struct Role: Hashable, Sendable {
        let role: String
        let label: String?
        let model: String?
        /// `chat`, `embedding`, `transcription` or `diarization`.
        let capability: String
        /// `app` (chosen for this app), `global`, `suggestion` or `none`.
        let source: String

        var title: String { label ?? role.capitalized }
    }

    let appID: String
    let name: String
    let trust: String
    let lastUsed: String?
    let roles: [Role]
    var id: String { appID }

    var verified: Bool { trust == "askantis" || trust == "signed" }

    init?(_ json: [String: Any]) {
        guard let appID = json["app_id"] as? String, let name = json["name"] as? String else { return nil }
        self.appID = appID
        self.name = name
        trust = json["trust"] as? String ?? "unsigned"
        lastUsed = json["last_used"] as? String
        roles = (json["roles"] as? [[String: Any]] ?? []).compactMap { r in
            guard let role = r["role"] as? String else { return nil }
            return Role(role: role, label: r["label"] as? String, model: r["model"] as? String,
                        capability: r["capability"] as? String ?? "", source: r["source"] as? String ?? "none")
        }
    }
}

/// A model in the service's inventory (PROTOCOL.md 7.1), for choosing a role's model.
struct InventoryModel: Identifiable, Hashable, Sendable {
    let id: String
    let displayName: String
    let capabilities: [String]
    let contextLength: Int?

    init?(_ json: [String: Any]) {
        guard let id = json["id"] as? String, (json["store"] as? String) != "app" else { return nil }
        self.id = id
        displayName = json["display_name"] as? String ?? id
        capabilities = json["capabilities"] as? [String] ?? []
        contextLength = json["context_length"] as? Int
    }

    /// Ollama names read better through ModelIdentity; speech models carry their own.
    var title: String { id.hasPrefix("hf:") ? displayName : ModelIdentity.prettyName(id) }
}

@MainActor @Observable
final class DaemonStore {
    static let shared = DaemonStore()

    static let plistName = "ch.askantis.localrouter.daemon.plist"

    private(set) var running = false
    private(set) var serviceStatus: SMAppService.Status = .notRegistered
    private(set) var apps: [ConnectedApp] = []
    /// Models a role can be bound to.
    private(set) var inventory: [InventoryModel] = []
    /// An app (and role) a deep link asked to show: `localrouter://apps?app=…&role=…`.
    private(set) var focus: (app: String, role: String?)?
    private(set) var focusToken = 0
    private(set) var lastError: String?
    private var started = false

    private var service: SMAppService { SMAppService.agent(plistName: Self.plistName) }

    /// Inside an app bundle only (not with `swift run`).
    var bundled: Bool {
        FileManager.default.fileExists(atPath: Bundle.main.bundleURL
            .appendingPathComponent("Contents/Library/LaunchAgents/\(Self.plistName)").path)
    }

    /// Follow the service's events, and register it, unless this is a first
    /// launch: then the welcome guide turns it on at its step, with an
    /// explanation for macOS's "background item added" notice.
    func start() {
        guard !started else { return }
        started = true
        if UserDefaults.standard.bool(forKey: SettingsKey.onboardingDone) || service.status != .notRegistered {
            ensureRegistered()
        } else {
            serviceStatus = service.status
        }
        Task { await refreshLoop() }
        Thread.detachNewThread { [weak self] in self?.followEvents() }
    }

    func ensureRegistered() {
        guard bundled else { return }
        serviceStatus = service.status
        Task { await register() }
    }

    /// Register the service, or register it again when launchd won't start
    /// it: launchd pins a registration to the code it was made for, so after
    /// an update (or a new build) the old registration fails with EX_CONFIG.
    /// Each step waits for the one before; launchd needs a moment to forget
    /// a registration, and registering again too early keeps the old one.
    private func register() async {
        if service.status != .enabled {
            do { try service.register() } catch { lastError = error.localizedDescription }
            serviceStatus = service.status
            return  // RunAtLoad starts it
        }
        if await Self.alive() {
            guard runningCopyIsOlder() else { return }
            // The copy from before an update: stop it so the new one runs.
            _ = try? await background { try UnixHTTP.request("POST", "/lr/v1/admin/shutdown") }
            await Self.waitUntil(seconds: 5) { !(await Self.alive()) }
        }
        try? await service.unregister()
        await Self.waitUntil(seconds: 10) { !(await Self.jobLoaded()) }
        try? await Task.sleep(for: .seconds(1))
        do { try service.register() } catch { lastError = error.localizedDescription }
        serviceStatus = service.status
        if !(await Self.waitUntil(seconds: 10) { await Self.alive() }) { Self.kickstart() }
    }

    /// Whether launchd still has the service's job.
    private static func jobLoaded() async -> Bool {
        (try? await background {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/bin/launchctl")
            p.arguments = ["print", "gui/\(getuid())/ch.askantis.localrouter.daemon"]
            p.standardOutput = FileHandle.nullDevice
            p.standardError = FileHandle.nullDevice
            try p.run()
            p.waitUntilExit()
            return p.terminationStatus == 0
        }) ?? false
    }

    @discardableResult
    private static func waitUntil(seconds: Double, _ check: () async -> Bool) async -> Bool {
        let end = Date().addingTimeInterval(seconds)
        while Date() < end {
            if await check() { return true }
            try? await Task.sleep(for: .milliseconds(250))
        }
        return await check()
    }

    /// The bundled service binary changed after the running service started.
    private func runningCopyIsOlder() -> Bool {
        let binary = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/localrouterd")
        guard let started = (Self.endpoint()?["started_at"] as? String).flatMap(Self.parseDate),
              let modified = (try? binary.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate
        else { return false }
        return modified > started
    }

    private static func parseDate(_ s: String) -> Date? {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f.date(from: s) ?? ISO8601DateFormatter().date(from: s)
    }

    private static func alive() async -> Bool {
        (try? await background { try UnixHTTP.request("GET", "/lr/v1/health") }) != nil
    }

    private static func kickstart() {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        p.arguments = ["kickstart", "gui/\(getuid())/ch.askantis.localrouter.daemon"]
        try? p.run()
    }

    func openLoginItems() {
        SMAppService.openSystemSettingsLoginItems()
    }

    /// "Stop Local Router": the service stops and doesn't come back until the app starts it again.
    func stopService() async {
        _ = try? await background { try UnixHTTP.request("POST", "/lr/v1/admin/shutdown") }
        try? await service.unregister()
        serviceStatus = service.status
        running = false
    }

    private func refreshLoop() async {
        while true {
            await refresh()
            try? await Task.sleep(for: .seconds(5))
        }
    }

    func refresh() async {
        serviceStatus = bundled ? service.status : serviceStatus
        let resp = try? await background { try UnixHTTP.request("GET", "/lr/v1/admin/apps") }
        running = resp != nil
        readEndpoint()
        guard let resp, resp.status == 200, let obj = resp.json as? [String: Any], let list = obj["apps"] as? [[String: Any]] else {
            if let resp, resp.status == 403 { lastError = "The service doesn't recognise this copy of Local Router." }
            return
        }
        lastError = nil
        apps = list.compactMap(ConnectedApp.init)
        if !apps.isEmpty,
           let models = try? await background({ try UnixHTTP.request("GET", "/lr/v1/models") }),
           let list = (models.json as? [String: Any])?["models"] as? [[String: Any]] {
            inventory = list.compactMap(InventoryModel.init)
        }
    }

    /// Models that can do what a role needs.
    func candidates(for role: ConnectedApp.Role) -> [InventoryModel] {
        inventory.filter { $0.capabilities.contains(role.capability) }
    }

    /// Choose a role's model for one app; `nil` goes back to automatic.
    func bind(_ app: ConnectedApp, role: String, model: String?) async {
        let appPath = app.appID.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? app.appID
        let rolePath = role.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? role
        _ = try? await background {
            let body: [String: Any] = ["model": model.map { $0 as Any } ?? NSNull()]
            return try UnixHTTP.request("PUT", "/lr/v1/admin/apps/\(appPath)/roles/\(rolePath)", json: body)
        }
        await refresh()
    }

    /// Show an app's row (from a deep link), for a few seconds.
    func focus(app: String, role: String?) {
        focus = (app, role)
        focusToken += 1
        let token = focusToken
        Task {
            try? await Task.sleep(for: .seconds(6))
            if focusToken == token { focus = nil }
        }
    }

    func disconnect(_ app: ConnectedApp) async {
        let path = "/lr/v1/admin/apps/" + (app.appID.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? app.appID)
        _ = try? await background { try UnixHTTP.request("DELETE", path) }
        await refresh()
    }

    /// A new `lra_` key for a tool that can only use TCP; the secret is shown once.
    func createKey(name: String) async -> String? {
        let resp = try? await background { try UnixHTTP.request("POST", "/lr/v1/admin/keys", json: ["name": name]) }
        return (resp?.json as? [String: Any])?["secret"] as? String
    }

    var tcpBaseURL: String {
        (Self.endpoint()?["http"] as? String) ?? "http://127.0.0.1:11436"
    }

    /// Where the service keeps speech models, and whether it can read them
    /// (PROTOCOL.md 2: ok, missing_volume, not_allowed, unwritable, checking).
    private(set) var hfCachePath: String?
    private(set) var hfCacheState: String?
    private var checkingSince: Date?

    /// Still "checking" after a few seconds: macOS is most likely asking
    /// whether the service may read the drive the models are on.
    var waitingForDrivePermission: Bool {
        guard hfCacheState == "checking", let since = checkingSince else { return false }
        return Date().timeIntervalSince(since) > 5
    }

    private static func endpoint() -> [String: Any]? {
        guard let data = try? Data(contentsOf: DaemonPaths.endpoint) else { return nil }
        return try? JSONSerialization.jsonObject(with: data) as? [String: Any]
    }

    private func readEndpoint() {
        let hf = Self.endpoint()?["hf_cache"] as? [String: Any]
        hfCachePath = hf?["path"] as? String
        let state = hf?["state"] as? String
        if state == "checking" { checkingSince = checkingSince ?? Date() } else { checkingSince = nil }
        hfCacheState = state
    }

    /// "Check Again" after allowing the drive: the service looks right away.
    func checkDriveAgain() async {
        _ = try? await background { try UnixHTTP.request("POST", "/lr/v1/admin/hf_cache/check") }
        try? await Task.sleep(for: .milliseconds(500))
        await refresh()
    }

    func openFilesAndFoldersSettings() {
        NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders")!)
    }

    // MARK: Connect requests (PROTOCOL.md 5.2)

    nonisolated private func followEvents() {
        while true {
            try? UnixHTTP.events { name, data in
                guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
                Task { @MainActor in self.handle(event: name, obj) }
            }
            Thread.sleep(forTimeInterval: 2)  // the service restarted, or isn't running yet
        }
    }

    private func handle(event: String, _ obj: [String: Any]) {
        switch event {
        case "connect.updated":
            guard let request = ConnectRequest(obj) else { return }
            if obj["status"] as? String == "pending" {
                guard !requests.contains(where: { $0.id == request.id }) else { return }
                requests.append(request)
                ApprovalPanel.shared.show()
                // The service lets a request wait 5 minutes (PROTOCOL.md 5.2).
                Task {
                    try? await Task.sleep(for: .seconds(5 * 60))
                    forget(request.id)
                }
            } else {
                forget(request.id)  // answered here, or elsewhere
                Task { await refresh() }
            }
        case "shutdown", "reset":
            // A new service forgot its requests.
            requests.removeAll()
            ApprovalPanel.shared.close()
            Task { await refresh() }
        case "hf_cache.state":
            Task { await refresh() }
        default:
            break
        }
    }

    /// Programs waiting for the user's answer, oldest first.
    private(set) var requests: [ConnectRequest] = []

    func answer(_ request: ConnectRequest, allow: Bool) {
        forget(request.id)
        Task {
            _ = try? await background { try UnixHTTP.request("POST", "/lr/v1/admin/connect/\(request.id)", json: ["approve": allow]) }
            await refresh()
        }
    }

    private func forget(_ id: String) {
        requests.removeAll { $0.id == id }
        if requests.isEmpty { ApprovalPanel.shared.close() }
    }
}

/// A program asking to connect (PROTOCOL.md 5.2). It's named the way its code
/// signature names it; what it says about itself is marked as not verified.
struct ConnectRequest: Identifiable, Hashable, Sendable {
    let id: String
    let name: String
    let verified: Bool
    let identifier: String?
    let path: String?
    let reason: String?
    let whyUnverified: String?

    init?(_ json: [String: Any]) {
        guard let id = json["request_id"] as? String else { return nil }
        self.id = id
        name = json["name"] as? String ?? "An app"
        verified = json["verified"] as? Bool ?? false
        identifier = json["identifier"] as? String
        path = json["path"] as? String
        reason = json["reason"] as? String
        whyUnverified = json["why_unverified"] as? String
    }

    var title: String {
        verified ? "“\(name)” wants to use your AI models" : "A program wants to use your AI models"
    }

    var details: [String] {
        if verified {
            return [reason, identifier.map { "Signed as \($0)." }].compactMap { $0 }
        }
        return ["It calls itself “\(name)”, but that isn't verified.", whyUnverified.map { "Not verified: \($0)." }].compactMap { $0 }
    }

    /// The program's own icon when it's an app, otherwise a generic one.
    @MainActor var icon: NSImage {
        guard let path else { return NSWorkspace.shared.icon(for: .unixExecutable) }
        var url = URL(fileURLWithPath: path)
        while url.pathComponents.count > 1, url.pathExtension != "app" { url.deleteLastPathComponent() }
        return NSWorkspace.shared.icon(forFile: url.pathExtension == "app" ? url.path : path)
    }
}

/// Asks about one request at a time without blocking the app: a modal alert
/// would stop everything else, including the next request and the menu.
@MainActor
final class ApprovalPanel {
    static let shared = ApprovalPanel()
    private var window: NSPanel?

    func show() {
        if window == nil {
            let host = NSHostingController(rootView: ApprovalView())
            host.sizingOptions = [.preferredContentSize]
            let panel = NSPanel(contentViewController: host)
            panel.styleMask = [.titled, .closable]
            panel.title = "Local Router"
            panel.level = .floating
            panel.isReleasedWhenClosed = false
            panel.hidesOnDeactivate = false
            window = panel
        }
        guard let window, !window.isVisible else { return }
        window.center()
        NSApp.activate()
        window.makeKeyAndOrderFront(nil)
    }

    func close() {
        window?.orderOut(nil)
    }
}

private struct ApprovalView: View {
    @State private var daemon = DaemonStore.shared

    var body: some View {
        if let request = daemon.requests.first {
            RequestCard(request: request, more: daemon.requests.count - 1)
                .padding(20)
                .frame(width: 420)
        }
    }
}

/// Used by the panel and by Settings → Apps.
struct RequestCard: View {
    let request: ConnectRequest
    var more = 0
    var compact = false

    var body: some View {
        HStack(alignment: .top, spacing: 14) {
            Image(nsImage: request.icon)
                .resizable()
                .frame(width: compact ? 32 : 48, height: compact ? 32 : 48)
                .overlay(alignment: .bottomTrailing) {
                    if !request.verified {
                        Image(systemName: "exclamationmark.triangle.fill")
                            .symbolRenderingMode(.multicolor)
                            .font(.system(size: compact ? 12 : 16))
                    }
                }
            VStack(alignment: .leading, spacing: 8) {
                Text(request.title).font(compact ? .system(size: 13, weight: .semibold) : .headline)
                ForEach(request.details, id: \.self) { Text($0).font(.callout).foregroundStyle(.secondary) }
                if let path = request.path, !request.verified {
                    Text(path)
                        .font(.system(size: 11, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .lineLimit(2)
                        .truncationMode(.middle)
                }
                if more > 0 {
                    Text(more == 1 ? "1 more program is waiting." : "\(more) more programs are waiting.")
                        .font(.callout).foregroundStyle(.secondary)
                }
                HStack {
                    Spacer()
                    // An unverified program is allowed only on purpose: Return means no.
                    if request.verified {
                        Button("Don't Allow") { DaemonStore.shared.answer(request, allow: false) }
                            .keyboardShortcut(.cancelAction)
                        Button("Allow") { DaemonStore.shared.answer(request, allow: true) }
                            .keyboardShortcut(.defaultAction)
                    } else {
                        Button("Allow") { DaemonStore.shared.answer(request, allow: true) }
                        Button("Don't Allow") { DaemonStore.shared.answer(request, allow: false) }
                            .keyboardShortcut(.defaultAction)
                    }
                }
                .padding(.top, 4)
            }
        }
    }
}

/// Run blocking socket work off the main actor.
private func background<T: Sendable>(_ work: @escaping @Sendable () throws -> T) async throws -> T {
    try await Task.detached(priority: .userInitiated) { try work() }.value
}

// MARK: - Settings → Apps

struct AppsPane: View {
    @State private var daemon = DaemonStore.shared
    @State private var keyName = ""
    @State private var newSecret: String?

    var body: some View {
        ScrollViewReader { proxy in
        Form {
            Section {
                LabeledContent("Background service") {
                    if daemon.running {
                        Label("Running", systemImage: "circle.fill").foregroundStyle(.green).labelStyle(.titleAndIcon)
                    } else if daemon.serviceStatus == .requiresApproval {
                        Button("Allow in Login Items…") { daemon.openLoginItems() }
                    } else {
                        Text("Not running").foregroundStyle(.secondary)
                    }
                }
                if daemon.running, let path = daemon.hfCachePath {
                    LabeledContent("Speech models") {
                        Text(Self.folderStatus(daemon.hfCacheState))
                            .foregroundStyle(daemon.hfCacheState == "ok" ? Color.secondary : Color.orange)
                    }
                    .help(path)
                    DriveAccessNote()
                }
                if let error = daemon.lastError {
                    Text(error).font(.callout).foregroundStyle(.orange)
                }
            } footer: {
                Footnote("Apps on this Mac use your models through Local Router's background service. It keeps running when you close the menu.")
            }

            if !daemon.requests.isEmpty {
                Section("Waiting for Your Answer") {
                    ForEach(daemon.requests) { RequestCard(request: $0, compact: true).padding(.vertical, 4) }
                }
            }

            Section("Connected Apps") {
                if daemon.apps.isEmpty {
                    Text("No apps yet. When an app like Scribely connects, it shows up here.")
                        .foregroundStyle(.secondary)
                }
                ForEach(daemon.apps) { app in
                    AppRow(app: app, focusedRole: daemon.focus?.app == app.appID ? .some(daemon.focus?.role) : nil) {
                        Task { await daemon.disconnect(app) }
                    }
                    .id(app.appID)
                }
            }

            Section {
                HStack {
                    TextField("Name", text: $keyName, prompt: Text("For example: Terminal"))
                    Button("Create Key") {
                        Task { newSecret = await daemon.createKey(name: keyName.isEmpty ? "Key" : keyName) }
                    }
                }
                if let secret = newSecret {
                    CopyableText(text: secret)
                    Text("Copy it now: Local Router shows a key only once.").font(.callout).foregroundStyle(.secondary)
                }
                LabeledContent("Base URL") { CopyableText(text: daemon.tcpBaseURL + "/v1") }
            } header: {
                Text("Connect Another App")
            } footer: {
                Footnote("For tools that can only use an OpenAI-compatible address. Apps that connect themselves don't need a key.")
            }
        }
        .onChange(of: daemon.focusToken) {
            if let app = daemon.focus?.app { withAnimation { proxy.scrollTo(app, anchor: .center) } }
        }
        }
        .task { await daemon.refresh() }
    }

    private static func folderStatus(_ state: String?) -> String {
        switch state {
        case "ok": "Ready"
        case "checking": "Checking…"
        case "missing_volume": "Drive not connected"
        case "not_allowed": "Not allowed"
        case "unwritable": "Can't write to the folder"
        default: "Unknown"
        }
    }
}

private struct AppRow: View {
    let app: ConnectedApp
    /// Set when a deep link asked for this app: the role it named, if any.
    let focusedRole: String??
    let disconnect: () -> Void
    @State private var daemon = DaemonStore.shared

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(app.name).font(.system(size: 13, weight: .semibold))
                if app.verified {
                    Label("Verified", systemImage: "checkmark.seal.fill").labelStyle(.iconOnly).foregroundStyle(.blue)
                        .help("Its code signature says who made it.")
                } else {
                    Text("Not verified").font(.system(size: 11)).foregroundStyle(.orange)
                }
                Spacer()
                Button("Disconnect", role: .destructive, action: disconnect).buttonStyle(.borderless)
            }
            Text(app.appID).font(.system(size: 11)).foregroundStyle(.secondary)
            // Roles the app declared (they carry a label), and any with a model;
            // not every well-known role an app could use.
            ForEach(app.roles.filter { $0.label != nil || $0.model != nil || focusedRole == .some($0.role) }, id: \.self) { role in
                RoleRow(app: app, role: role, highlighted: focusedRole == .some(role.role))
            }
        }
        .padding(.vertical, 2)
        .padding(.horizontal, focusedRole != nil ? 6 : 0)
        .background(focusedRole != nil ? Color.accentColor.opacity(0.10) : .clear, in: RoundedRectangle(cornerRadius: 8))
        .animation(.easeOut(duration: 0.3), value: focusedRole != nil)
    }
}

/// One job an app needs a model for, and the model it gets.
private struct RoleRow: View {
    let app: ConnectedApp
    let role: ConnectedApp.Role
    let highlighted: Bool
    @State private var daemon = DaemonStore.shared

    var body: some View {
        HStack {
            Text(role.title)
                .font(.system(size: 12, weight: highlighted ? .semibold : .regular))
            Spacer()
            let candidates = daemon.candidates(for: role)
            if candidates.isEmpty && role.model == nil {
                Text("No model for this yet").font(.system(size: 12)).foregroundStyle(.secondary)
            } else {
                Menu {
                    Button("Automatic") { Task { await daemon.bind(app, role: role.role, model: nil) } }
                    if !candidates.isEmpty { Divider() }
                    ForEach(candidates) { m in
                        Button {
                            Task { await daemon.bind(app, role: role.role, model: m.id) }
                        } label: {
                            if role.source == "app" && role.model == m.id {
                                Label(m.title, systemImage: "checkmark")
                            } else {
                                Text(m.title)
                            }
                        }
                    }
                } label: {
                    Text(current).font(.system(size: 12))
                }
                .menuStyle(.borderlessButton)
                .fixedSize()
                .help("Which model \(app.name) uses for \(role.title.lowercased()).")
            }
        }
    }

    /// "Qwen 3.5 9B", or "Automatic: Qwen 3.5 9B" when Local Router chose it.
    private var current: String {
        guard let model = role.model else { return "Automatic" }
        let title = daemon.inventory.first { $0.id == model }?.title ?? ModelIdentity.prettyName(model)
        return role.source == "app" ? title : "Automatic: \(title)"
    }
}
