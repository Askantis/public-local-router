import CryptoKit
import Foundation
import Network

/// An access key for Sharing. Only its SHA-256 hash is stored; the key itself
/// is shown once, when it's created.
struct AccessKey: Codable, Identifiable, Equatable {
    var id = UUID()
    var name: String
    var hash: String
    var created = Date.now
    var lastUsed: Date?

    /// "lr_" + 32 random bytes, URL-safe base64.
    static func generate(name: String) -> (key: AccessKey, secret: String) {
        var bytes = [UInt8](repeating: 0, count: 32)
        _ = SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes)
        let secret = "lr_" + Data(bytes).base64EncodedString()
            .replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
        return (AccessKey(name: name, hash: digest(secret)), secret)
    }

    static func digest(_ secret: String) -> String {
        SHA256.hash(data: Data(secret.utf8)).map { String(format: "%02x", $0) }.joined()
    }
}

/// Listens on the network and forwards requests that carry a valid access key
/// (`Authorization: Bearer lr_…`) to Ollama on this Mac. Ollama itself stays
/// local-only. One request per connection, so every request is checked.
final class ShareProxy: @unchecked Sendable {
    struct Event: Sendable {
        let client: String
        let keyID: UUID?
        let keyName: String?
        let method: String
        let path: String
        let accepted: Bool
    }

    private let queue = DispatchQueue(label: "ch.askantis.localrouter.share")
    private var listener: NWListener?
    /// Hash → key. Replaced as a whole from the main thread, read on `queue`.
    private var keys: [String: (id: UUID, name: String)] = [:]
    private let upstreamHost: String
    private let upstreamPort: UInt16
    private let onEvent: @Sendable (Event) -> Void

    init(upstreamHost: String, upstreamPort: UInt16, onEvent: @escaping @Sendable (Event) -> Void) {
        self.upstreamHost = upstreamHost
        self.upstreamPort = upstreamPort
        self.onEvent = onEvent
    }

    func setKeys(_ keys: [AccessKey]) {
        let map = Dictionary(keys.map { ($0.hash, (id: $0.id, name: $0.name)) }, uniquingKeysWith: { first, _ in first })
        queue.async { self.keys = map }
    }

    /// `localOnly` binds to 127.0.0.1, for testing.
    /// With `identity`, connections are HTTPS (TLS) with that certificate.
    func start(port: UInt16, identity: SecIdentity? = nil, localOnly: Bool = false,
               stateChanged: @escaping @Sendable (NWListener.State) -> Void) throws {
        let parameters: NWParameters
        if let identity, let secIdentity = sec_identity_create(identity) {
            let tls = NWProtocolTLS.Options()
            sec_protocol_options_set_local_identity(tls.securityProtocolOptions, secIdentity)
            sec_protocol_options_set_min_tls_protocol_version(tls.securityProtocolOptions, .TLSv12)
            parameters = NWParameters(tls: tls)
        } else {
            parameters = .tcp
        }
        parameters.allowLocalEndpointReuse = true
        if localOnly {
            parameters.requiredLocalEndpoint = NWEndpoint.hostPort(host: "127.0.0.1", port: NWEndpoint.Port(rawValue: port)!)
        }
        let listener = try localOnly ? NWListener(using: parameters) : NWListener(using: parameters, on: NWEndpoint.Port(rawValue: port)!)
        listener.stateUpdateHandler = stateChanged
        listener.newConnectionHandler = { [weak self] connection in self?.accept(connection) }
        listener.start(queue: queue)
        self.listener = listener
    }

    /// Stops listening and closes every open connection, so turning sharing
    /// off also ends streams that are running.
    func stop() {
        queue.async {
            self.listener?.cancel()
            self.listener = nil
            for connection in self.open.values { connection.cancel() }
            self.open.removeAll()
        }
    }

    // MARK: - Connections

    /// Every live connection (client and Ollama side), to close them on stop.
    private var open: [ObjectIdentifier: NWConnection] = [:]

    private func track(_ connection: NWConnection) {
        let id = ObjectIdentifier(connection)
        open[id] = connection
        connection.stateUpdateHandler = { [weak self] state in
            guard let proxy = self else { return }
            switch state {
            case .cancelled, .failed: proxy.queue.async { proxy.open[id] = nil }
            default: break
            }
        }
    }

    private func accept(_ client: NWConnection) {
        track(client)
        client.start(queue: queue)
        // A request's headers must arrive within 10 s.
        queue.asyncAfter(deadline: .now() + 10) { [weak self] in
            guard let self, self.waitingForHead.contains(ObjectIdentifier(client)) else { return }
            client.cancel()
        }
        waitingForHead.insert(ObjectIdentifier(client))
        readHead(client, buffer: Data())
    }

    private var waitingForHead: Set<ObjectIdentifier> = []

    /// Reads until the end of the request headers (at most 64 KB).
    private func readHead(_ client: NWConnection, buffer: Data) {
        client.receive(minimumIncompleteLength: 1, maximumLength: 65_536) { [weak self] data, _, isComplete, error in
            guard let self else { client.cancel(); return }
            var buffer = buffer
            if let data { buffer.append(data) }
            if let end = buffer.range(of: Data("\r\n\r\n".utf8)) {
                self.waitingForHead.remove(ObjectIdentifier(client))
                self.handle(client, head: buffer[..<end.lowerBound], rest: Data(buffer[end.upperBound...]))
            } else if buffer.count > 65_536 || isComplete || error != nil {
                self.waitingForHead.remove(ObjectIdentifier(client))
                client.cancel()
            } else {
                self.readHead(client, buffer: buffer)
            }
        }
    }

    private func handle(_ client: NWConnection, head: Data, rest: Data) {
        let lines = String(decoding: head, as: UTF8.self).components(separatedBy: "\r\n")
        let requestLine = lines.first?.split(separator: " ").map(String.init) ?? []
        guard requestLine.count == 3, requestLine[2].hasPrefix("HTTP/1.") else {
            return reject(client, status: "400 Bad Request", message: "bad request")
        }
        let method = requestLine[0], path = requestLine[1]
        let address = Self.address(of: client)

        var headers: [(String, String)] = []
        for line in lines.dropFirst() {
            guard let colon = line.firstIndex(of: ":") else { continue }
            headers.append((String(line[..<colon]), line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)))
        }
        func header(_ name: String) -> [String] { headers.filter { $0.0.lowercased() == name }.map(\.1) }

        // Browsers ask first with OPTIONS, without the key.
        if method == "OPTIONS" {
            let reply = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nAccess-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\nConnection: close\r\n\r\n"
            client.send(content: Data(reply.utf8), completion: .contentProcessed { _ in client.cancel() })
            return
        }

        let secret = header("authorization").first.flatMap { $0.lowercased().hasPrefix("bearer ") ? String($0.dropFirst(7)) : nil } ?? ""
        let hash = AccessKey.digest(secret)
        // Constant-time compare against every key, so timing doesn't leak which is close.
        var match: (id: UUID, name: String)?
        for (stored, key) in keys where Self.constantTimeEqual(stored, hash) { match = key }

        guard !secret.isEmpty, let match else {
            onEvent(Event(client: address, keyID: nil, keyName: nil, method: method, path: path, accepted: false))
            return reject(client, status: "401 Unauthorized",
                          message: secret.isEmpty ? "missing API key: send Authorization: Bearer <key>" : "invalid API key")
        }

        // Only a plain Content-Length body, so exactly one request is forwarded
        // per connection and nothing can be smuggled behind it.
        guard header("transfer-encoding").isEmpty else {
            return reject(client, status: "411 Length Required", message: "send the body with Content-Length")
        }
        let lengths = Set(header("content-length"))
        guard lengths.count <= 1, let bodyLength = Int(lengths.first ?? "0"), bodyLength >= 0 else {
            return reject(client, status: "400 Bad Request", message: "bad Content-Length")
        }
        onEvent(Event(client: address, keyID: match.id, keyName: match.name, method: method, path: path, accepted: true))

        // Forward without the key or the browser's origin (Ollama would refuse
        // it), to Ollama, closing after this request.
        var forwarded = "\(requestLine.joined(separator: " "))\r\n"
        for (name, value) in headers {
            switch name.lowercased() {
            case "authorization", "connection", "host", "keep-alive", "proxy-connection", "origin", "content-length": continue
            default: forwarded += "\(name): \(value)\r\n"
            }
        }
        forwarded += "Content-Length: \(bodyLength)\r\nHost: \(upstreamHost):\(upstreamPort)\r\nConnection: close\r\n\r\n"
        let firstBody = rest.prefix(bodyLength)
        let firstPacket = Data(forwarded.utf8) + firstBody
        let remaining = bodyLength - firstBody.count

        let upstream = NWConnection(host: NWEndpoint.Host(upstreamHost), port: NWEndpoint.Port(rawValue: upstreamPort)!, using: .tcp)
        track(upstream)
        let tracked = upstream.stateUpdateHandler
        upstream.stateUpdateHandler = { [weak self] state in
            tracked?(state)
            switch state {
            case .ready:
                upstream.send(content: firstPacket, completion: .contentProcessed { _ in })
                self?.forwardBody(from: client, to: upstream, remaining: remaining)
                self?.relayResponse(from: upstream, to: client, first: true)
            case .failed, .waiting:
                self?.reject(client, status: "502 Bad Gateway", message: "Ollama isn't reachable")
                upstream.cancel()
            default: break
            }
        }
        upstream.start(queue: queue)

        // If the device goes away, stop the request in Ollama too.
        let clientTracked = client.stateUpdateHandler
        client.stateUpdateHandler = { state in
            clientTracked?(state)
            switch state {
            case .failed, .cancelled: upstream.cancel()
            default: break
            }
        }
    }

    /// Sends exactly the rest of the declared body, then stops reading from the device.
    private func forwardBody(from client: NWConnection, to upstream: NWConnection, remaining: Int) {
        guard remaining > 0 else { return }
        client.receive(minimumIncompleteLength: 1, maximumLength: min(remaining, 262_144)) { [weak self] data, _, isComplete, error in
            if let data, !data.isEmpty {
                let chunk = data.prefix(remaining)
                upstream.send(content: chunk, completion: .contentProcessed { _ in })
                if isComplete || error != nil { return }
                self?.forwardBody(from: client, to: upstream, remaining: remaining - chunk.count)
            } else if error != nil {
                upstream.cancel()
            }
        }
    }

    /// Copies Ollama's response to the device as it streams, adding a CORS
    /// header so browser apps on other devices can read it.
    private func relayResponse(from upstream: NWConnection, to client: NWConnection, first: Bool) {
        upstream.receive(minimumIncompleteLength: 1, maximumLength: 262_144) { [weak self] data, _, isComplete, error in
            if var data, !data.isEmpty {
                if first, let lineEnd = data.range(of: Data("\r\n".utf8)) {
                    data.insert(contentsOf: Data("Access-Control-Allow-Origin: *\r\n".utf8), at: lineEnd.upperBound)
                }
                client.send(content: data, completion: .contentProcessed { _ in })
            }
            if isComplete || error != nil {
                client.send(content: nil, isComplete: true, completion: .contentProcessed { _ in
                    client.cancel()
                    upstream.cancel()
                })
                return
            }
            self?.relayResponse(from: upstream, to: client, first: false)
        }
    }

    private func reject(_ client: NWConnection, status: String, message: String) {
        let body = #"{"error":"\#(message)"}"#
        let reply = "HTTP/1.1 \(status)\r\nContent-Type: application/json\r\nWWW-Authenticate: Bearer\r\nContent-Length: \(body.utf8.count)\r\nConnection: close\r\n\r\n\(body)"
        client.send(content: Data(reply.utf8), completion: .contentProcessed { _ in client.cancel() })
    }

    private static func address(of connection: NWConnection) -> String {
        if case .hostPort(let host, _) = connection.endpoint {
            return "\(host)".components(separatedBy: "%").first ?? "\(host)"
        }
        return "unknown"
    }

    private static func constantTimeEqual(_ a: String, _ b: String) -> Bool {
        let x = Array(a.utf8), y = Array(b.utf8)
        guard x.count == y.count else { return false }
        return zip(x, y).reduce(0) { $0 | ($1.0 ^ $1.1) } == 0
    }
}

/// This Mac's addresses on the local network, for showing how to connect.
enum LocalAddresses {
    static func ipv4() -> [String] {
        var result: [String] = []
        var interfaces: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&interfaces) == 0, let first = interfaces else { return [] }
        defer { freeifaddrs(interfaces) }
        for pointer in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let interface = pointer.pointee
            guard let address = interface.ifa_addr, address.pointee.sa_family == UInt8(AF_INET),
                  String(cString: interface.ifa_name).hasPrefix("en") else { continue }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            if getnameinfo(address, socklen_t(address.pointee.sa_len), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0 {
                result.append(String(decoding: host.prefix { $0 != 0 }.map(UInt8.init), as: UTF8.self))
            }
        }
        return result
    }

    static var hostName: String {
        let name = ProcessInfo.processInfo.hostName
        return name.hasSuffix(".local") ? name : name + ".local"
    }
}

/// The self-signed certificate for HTTPS sharing. Created once with the
/// system's openssl, kept in Application Support (readable only by you).
enum ShareCertificate {
    static var folder: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appending(path: "Local Router", directoryHint: .isDirectory)
    }
    static var certificateURL: URL { folder.appending(path: "sharing-certificate.pem") }
    private static var bundleURL: URL { folder.appending(path: "sharing-identity.p12") }

    /// The identity (certificate + key), creating it the first time. If the
    /// file can't be opened (e.g. made with another password), it's replaced.
    static func identity() throws -> SecIdentity {
        if !FileManager.default.fileExists(atPath: bundleURL.path) { try create() }
        if let identity = try? load() { return identity }
        try regenerate()
        return try load()
    }

    private static func load() throws -> SecIdentity {
        let data = try Data(contentsOf: bundleURL)
        var items: CFArray?
        let options = [kSecImportExportPassphrase as String: password, kSecImportToMemoryOnly as String: true] as CFDictionary
        let status = SecPKCS12Import(data as CFData, options, &items)
        guard status == errSecSuccess, let first = (items as? [[String: Any]])?.first,
              let identity = first[kSecImportItemIdentity as String] else {
            throw ModelFolderMover.MoveError(message: "Couldn't read the sharing certificate (error \(status)).")
        }
        return identity as! SecIdentity
    }

    /// Makes a new certificate, for this Mac's name and current addresses.
    static func create() throws {
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let key = folder.appending(path: "sharing-key.pem")
        var names = ["DNS:localhost", "DNS:\(LocalAddresses.hostName)", "IP:127.0.0.1"]
        names += LocalAddresses.ipv4().map { "IP:\($0)" }
        _ = OllamaRunner.shell("/usr/bin/openssl", [
            "req", "-x509", "-newkey", "rsa:2048", "-sha256", "-nodes", "-days", "825",
            "-subj", "/CN=Local Router on \(LocalAddresses.hostName)",
            "-addext", "subjectAltName=\(names.joined(separator: ","))",
            "-keyout", key.path, "-out", certificateURL.path,
        ])
        _ = OllamaRunner.shell("/usr/bin/openssl", [
            "pkcs12", "-export", "-inkey", key.path, "-in", certificateURL.path,
            "-out", bundleURL.path, "-passout", "pass:\(password)",
        ])
        try? FileManager.default.removeItem(at: key) // the .p12 holds it now
        for file in [bundleURL, certificateURL] {
            try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: file.path)
        }
        guard FileManager.default.fileExists(atPath: bundleURL.path) else {
            throw ModelFolderMover.MoveError(message: "Couldn't create the sharing certificate with /usr/bin/openssl.")
        }
    }

    static func regenerate() throws {
        try? FileManager.default.removeItem(at: bundleURL)
        try? FileManager.default.removeItem(at: certificateURL)
        try create()
    }

    /// SHA-256 of the certificate, to compare on another device.
    static var fingerprint: String? {
        guard let pem = try? String(contentsOf: certificateURL, encoding: .utf8) else { return nil }
        let base64 = pem.components(separatedBy: "\n").filter { !$0.hasPrefix("-----") }.joined()
        guard let der = Data(base64Encoded: base64) else { return nil }
        return SHA256.hash(data: der).map { String(format: "%02X", $0) }.joined(separator: ":")
    }

    /// Protects the .p12 on disk; the file itself is readable only by you.
    private static var password: String {
        if let saved = UserDefaults.standard.string(forKey: "sharingCertificatePassword") { return saved }
        let new = UUID().uuidString
        UserDefaults.standard.set(new, forKey: "sharingCertificatePassword")
        return new
    }
}
