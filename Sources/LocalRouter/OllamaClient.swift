import Foundation

struct ModelDetails: Decodable, Hashable, Sendable {
    let format: String?
    let family: String?
    /// The most context the model supports.
    let contextLength: Int?
    let parameterSize: String?
    let quantizationLevel: String?
}

/// An installed model, from `GET /api/tags`.
struct LocalModel: Decodable, Identifiable, Hashable, Sendable {
    let name: String
    let size: Int64
    let details: ModelDetails?
    let capabilities: [String]?
    var id: String { name }

    var isMLX: Bool { details?.format == "safetensors" || name.contains("mlx") }
}

/// One line of `POST /api/pull`'s progress stream.
struct PullUpdate: Decodable, Sendable {
    let status: String?
    /// The layer (weights, template…) this update is about.
    let digest: String?
    let total: Int64?
    let completed: Int64?
    let error: String?
}

/// A loaded model, from `GET /api/ps`.
struct RunningModel: Decodable, Identifiable, Hashable, Sendable {
    let name: String
    let size: Int64
    let expiresAt: Date?
    let contextLength: Int?
    let details: ModelDetails?
    var id: String { name }

    /// Ollama reports `keep_alive: -1` as an expiry hundreds of years away.
    var staysLoaded: Bool {
        guard let expiresAt else { return false }
        return expiresAt.timeIntervalSinceNow > 10 * 365 * 24 * 3600
    }
}

/// How long a model stays loaded after its last use (Ollama's `keep_alive`).
enum KeepAlive: String, CaseIterable, Identifiable, Sendable {
    case fiveMinutes = "5m"
    case fifteenMinutes = "15m"
    case oneHour = "1h"
    case fourHours = "4h"
    case untilEjected = "-1"

    var id: String { rawValue }

    var label: String {
        switch self {
        case .fiveMinutes: "5 minutes (Ollama default)"
        case .fifteenMinutes: "15 minutes"
        case .oneHour: "1 hour"
        case .fourHours: "4 hours"
        case .untilEjected: "Until ejected"
        }
    }

    var jsonValue: Any { self == .untilEjected ? -1 : rawValue }
}

struct OllamaError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

struct OllamaClient: Sendable {
    let baseURL: URL

    func version() async throws -> String {
        struct Response: Decodable { let version: String }
        return try await get("api/version", as: Response.self).version
    }

    func installedModels() async throws -> [LocalModel] {
        struct Response: Decodable { let models: [LocalModel] }
        return try await get("api/tags", as: Response.self).models
    }

    func loadedModels() async throws -> [RunningModel] {
        struct Response: Decodable { let models: [RunningModel] }
        return try await get("api/ps", as: Response.self).models
    }

    /// The model's Modelfile, whose FROM line names its weights file.
    func modelfile(_ model: String) async throws -> String {
        var request = URLRequest(url: baseURL.appending(path: "api/show"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: ["model": model])
        request.timeoutInterval = 3
        let (data, response) = try await URLSession.shared.data(for: request)
        try check(response, data)
        struct Response: Decodable { let modelfile: String? }
        return try JSONDecoder().decode(Response.self, from: data).modelfile ?? ""
    }

    /// A generate request without a prompt only loads the model.
    /// With `contextLength`, the model loads with that `num_ctx`. Keep sending
    /// the same value: a different one makes Ollama reload the model.
    /// Embedding-only models reject /api/generate, so they load through /api/embed.
    func load(_ model: String, keepAlive: KeepAlive, contextLength: Int? = nil, embedding: Bool = false) async throws {
        var body: [String: Any] = ["model": model, "keep_alive": keepAlive.jsonValue]
        if let contextLength { body["options"] = ["num_ctx": contextLength] }
        if embedding {
            body["input"] = [String]()
            try await post("api/embed", body: body)
        } else {
            body["stream"] = false
            try await post("api/generate", body: body)
        }
    }

    func eject(_ model: String, embedding: Bool = false) async throws {
        if embedding {
            try await post("api/embed", body: ["model": model, "keep_alive": 0, "input": [String]()])
        } else {
            try await post("api/generate", body: ["model": model, "keep_alive": 0, "stream": false])
        }
    }

    /// Removes the model from disk (`ollama rm`).
    func delete(_ model: String) async throws {
        try await post("api/delete", body: ["model": model], method: "DELETE")
    }

    /// Downloads a model, reporting progress. Cancelling the task stops the download.
    func pull(_ model: String, onUpdate: @Sendable (PullUpdate) async -> Void) async throws {
        var request = URLRequest(url: baseURL.appending(path: "api/pull"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: ["model": model, "stream": true])
        request.timeoutInterval = 3600
        let (bytes, response) = try await URLSession.shared.bytes(for: request)
        if let http = response as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            throw OllamaError(message: "Ollama returned HTTP \(http.statusCode)")
        }
        for try await line in bytes.lines {
            guard let update = try? JSONDecoder().decode(PullUpdate.self, from: Data(line.utf8)) else { continue }
            if let error = update.error { throw OllamaError(message: error) }
            await onUpdate(update)
        }
    }

    // MARK: - HTTP

    private func get<T: Decodable>(_ path: String, as type: T.Type) async throws -> T {
        var request = URLRequest(url: baseURL.appending(path: path))
        request.timeoutInterval = 3
        let (data, response) = try await URLSession.shared.data(for: request)
        try check(response, data)
        return try Self.decoder().decode(T.self, from: data)
    }

    private func post(_ path: String, body: [String: Any], method: String = "POST") async throws {
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
        // Loading a big model from an external drive can take minutes.
        request.timeoutInterval = 600
        let (data, response) = try await URLSession.shared.data(for: request)
        try check(response, data)
    }

    private func check(_ response: URLResponse, _ data: Data) throws {
        guard let http = response as? HTTPURLResponse, !(200..<300).contains(http.statusCode) else { return }
        struct ErrorBody: Decodable { let error: String }
        let message = (try? JSONDecoder().decode(ErrorBody.self, from: data))?.error
        throw OllamaError(message: message ?? "Ollama returned HTTP \(http.statusCode)")
    }

    private static func decoder() -> JSONDecoder {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        decoder.dateDecodingStrategy = .custom { decoder in
            let string = try decoder.singleValueContainer().decode(String.self)
            // Ollama sends nanosecond fractions, which ISO8601DateFormatter can't parse.
            let trimmed = string.replacingOccurrences(of: #"\.\d+"#, with: "", options: .regularExpression)
            guard let date = ISO8601DateFormatter().date(from: trimmed) else {
                throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "Bad date: \(string)"))
            }
            return date
        }
        return decoder
    }
}
