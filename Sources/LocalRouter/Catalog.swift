import Foundation

/// One downloadable build of a library model, e.g. `qwen3.5:9b` or `qwen3.5:9b-mlx`.
struct CatalogVariant: Decodable, Hashable, Identifiable, Sendable {
    let tag: String
    let size: Int64
    let context: Int?
    let mlx: Bool
    let inputs: [String]
    let isDefault: Bool
    var id: String { tag }

    /// "9B", "35B A3B", "E4B"
    var sizeLabel: String {
        ModelIdentity.prettyName(tag.replacingOccurrences(of: "-mlx", with: ""))
    }
}

/// A model from ollama.com/library, bundled as Resources/catalog.json
/// (built by scripts/update-catalog.py).
struct CatalogModel: Decodable, Hashable, Identifiable, Sendable {
    let name: String
    let description: String
    let capabilities: [String]
    let pulls: Int
    let updated: String?
    let maker: String?
    let uses: [String]
    let variants: [CatalogVariant]
    var id: String { name }

    var title: String { ModelIdentity.prettyName(name) }
    var capabilityList: [Capability] { Capability.from(capabilities) }
    var useList: [ModelTag] { ModelTag.allCases.filter { uses.contains($0.rawValue) } }

    var formats: [ModelFormat] {
        var result: [ModelFormat] = []
        if variants.contains(where: { !$0.mlx }) { result.append(.gguf) }
        if variants.contains(where: \.mlx) { result.append(.mlx) }
        return result
    }

    func pullName(_ variant: CatalogVariant) -> String { "\(name):\(variant.tag)" }

    /// Popularity with a bonus for recent updates, so current models beat
    /// old ones that collected more downloads over the years.
    var score: Double {
        let popularity = log10(Double(pulls) + 10)
        guard let updated, let date = ISO8601DateFormatter.dateOnly.date(from: updated) else { return popularity }
        let months = Date.now.timeIntervalSince(date) / (30 * 24 * 3600)
        return popularity + max(0, 3 - months / 4)
    }

    /// The largest build that runs comfortably, else the largest that runs at all,
    /// else the smallest. Without a format filter it picks the preferred format
    /// (MLX on Apple silicon if "Prefer MLX builds" is on, else GGUF), and falls
    /// back to the other one when the model has no build in that format.
    func bestVariant(on machine: MachineInfo?, format: ModelFormat? = nil, sizeRange: ClosedRange<Int64>? = nil,
                     preferMLX: Bool = false) -> CatalogVariant? {
        let inRange = variants.filter { sizeRange?.contains($0.size) ?? true }
        let wanted = format ?? (preferMLX ? .mlx : .gguf)
        let matching = inRange.filter { $0.mlx == (wanted == .mlx) }
        let candidates = format != nil || !matching.isEmpty ? matching : inRange
        guard let machine else { return candidates.first(where: \.isDefault) ?? candidates.first }
        let bySize = candidates.sorted { $0.size < $1.size }
        return bySize.last(where: { machine.fit(for: $0.size) >= .good })
            ?? bySize.last(where: { machine.fit(for: $0.size) >= .tight })
            ?? bySize.first
    }
}

enum Catalog {
    static let models: [CatalogModel] = {
        guard let url = Bundle.main.url(forResource: "catalog", withExtension: "json"),
              let data = try? Data(contentsOf: url) else { return [] }
        struct File: Decodable { let models: [CatalogModel] }
        return (try? JSONDecoder().decode(File.self, from: data).models) ?? []
    }()

    static let generatedAt: String? = {
        guard let url = Bundle.main.url(forResource: "catalog", withExtension: "json"),
              let data = try? Data(contentsOf: url),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        return json["generatedAt"] as? String
    }()
}

extension ISO8601DateFormatter {
    nonisolated(unsafe) static let dateOnly: ISO8601DateFormatter = {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withFullDate]
        return formatter
    }()
}
