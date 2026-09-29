import AppKit
import SwiftUI

/// Readable names and maker icons for Ollama model names.
enum ModelIdentity {
    /// `glm-4.7-flash:latest` → "GLM 4.7 Flash", `qwen3.8:27b` → "Qwen 3.8 27B",
    /// `MichelRosselli/apertus:8b-instruct-2509-q4_k_m` → "Apertus 8B 2509".
    /// Quantisation and format suffixes are dropped; the details line shows what matters.
    static func prettyName(_ name: String) -> String {
        var text = (name.split(separator: "/").last.map(String.init) ?? name).lowercased()
        text = text.replacingOccurrences(of: ":latest", with: "")
        text = text.replacingOccurrences(
            of: #"(?:^|[-_:.])(i?q\d(?:_[a-z0-9]+)*|fp\d+|bf16|f16|f32|mxfp\d+|nvfp\d+|int\d+|gguf|mlx)(?=$|[-_:])"#,
            with: "", options: .regularExpression)

        var words: [String] = []
        for raw in text.split(whereSeparator: { "-_:".contains($0) }) {
            let word = String(raw)
            if ["instruct", "it", "chat", "latest"].contains(word) { continue }
            for part in format(word) where words.last != part {
                words.append(part)
            }
        }
        let joined = words.joined(separator: " ").replacingOccurrences(of: "GPT OSS", with: "GPT-OSS")
        return joined.isEmpty ? name : joined
    }

    private static let specials: [String: String] = [
        "glm": "GLM", "gpt": "GPT", "oss": "OSS", "vl": "VL", "ocr": "OCR", "r1": "R1", "moe": "MoE",
        "llava": "LLaVA", "qwq": "QwQ", "deepseek": "DeepSeek", "minicpm": "MiniCPM", "smollm": "SmolLM",
        "olmo": "OLMo", "lfm": "LFM", "exaone": "EXAONE", "codellama": "Code Llama", "starcoder": "StarCoder",
        "internlm": "InternLM", "codegemma": "CodeGemma", "gptoss": "GPT-OSS",
    ]

    private static func format(_ word: String) -> [String] {
        if let special = specials[word] { return [special] }
        if word.range(of: #"^[aem]?\d+(\.\d+)?[bmk]$"#, options: .regularExpression) != nil { return [word.uppercased()] }
        if word.range(of: #"^(v\d+(\.\d+)?|[\d.]+)$"#, options: .regularExpression) != nil { return [word] }
        // "qwen3.5" → "Qwen 3.5", "qwen2.5vl" → "Qwen 2.5 VL"
        if let match = word.firstMatch(of: /^([a-z]+)(\d[\d.]*)([a-z]*)$/) {
            var parts = [capitalized(String(match.1)), String(match.2)]
            if !match.3.isEmpty { parts.append(capitalized(String(match.3))) }
            return parts
        }
        return [capitalized(word)]
    }

    private static func capitalized(_ word: String) -> String {
        specials[word] ?? (word.prefix(1).uppercased() + word.dropFirst())
    }

    /// Library name prefix → Hugging Face organisation whose avatar is bundled
    /// in Resources/icons. Keep in sync with scripts/update-catalog.py.
    private static let makers: [(String, String)] = [
        ("qwen", "Qwen"), ("qwq", "Qwen"), ("codegemma", "google"), ("gemma", "google"),
        ("codellama", "meta-llama"), ("llama", "meta-llama"), ("muse", "meta-llama"),
        ("mistral", "mistralai"), ("mixtral", "mistralai"), ("devstral", "mistralai"), ("magistral", "mistralai"),
        ("ministral", "mistralai"), ("codestral", "mistralai"),
        ("gpt-oss", "openai"), ("gptoss", "openai"), ("deepseek", "deepseek-ai"), ("phi", "microsoft"),
        ("glm", "zai-org"), ("granite", "ibm-granite"), ("olmo", "allenai"), ("smollm", "HuggingFaceTB"),
        ("nemotron", "nvidia"), ("command", "CohereLabs"), ("aya", "CohereLabs"), ("falcon", "tiiuae"),
        ("kimi", "moonshotai"), ("minimax", "MiniMaxAI"), ("apertus", "swiss-ai"), ("hermes", "NousResearch"),
        ("lfm", "LiquidAI"), ("exaone", "LGAI-EXAONE"), ("starcoder", "bigcode"), ("minicpm", "openbmb"),
        ("internlm", "internlm"), ("yi", "01-ai"), ("seed", "ByteDance-Seed"), ("cogito", "deepcogito"),
    ].sorted { $0.0.count > $1.0.count }

    static func maker(for name: String, family: String? = nil) -> String? {
        let base = (name.split(separator: "/").last.map(String.init) ?? name).lowercased()
        for candidate in [base, family?.lowercased() ?? ""] where !candidate.isEmpty {
            if let match = makers.first(where: { candidate.hasPrefix($0.0) }) { return match.1 }
        }
        return nil
    }
}

enum Capability: String, CaseIterable, Identifiable {
    case vision, thinking, tools, audio

    var id: String { rawValue }

    var label: String {
        switch self {
        case .vision: "Vision"
        case .thinking: "Thinking"
        case .tools: "Tool Use"
        case .audio: "Audio"
        }
    }

    var systemImage: String {
        switch self {
        case .vision: "eye"
        case .thinking: "brain"
        case .tools: "wrench.and.screwdriver"
        case .audio: "waveform"
        }
    }

    // Close to LM Studio's badges: vision amber-orange, tools blue, thinking purple.
    var color: Color {
        switch self {
        case .vision: .orange
        case .thinking: .purple
        case .tools: .blue
        case .audio: .pink
        }
    }

    static func from(_ raw: [String]?) -> [Capability] {
        let set = Set(raw ?? [])
        return allCases.filter { set.contains($0.rawValue) }
    }
}

enum ModelFormat: String, CaseIterable, Identifiable {
    case gguf, mlx

    var id: String { rawValue }
    var label: String { rawValue.uppercased() }
    var systemImage: String { self == .mlx ? "apple.logo" : "shippingbox" }
    /// MLX is Apple's, so it gets silver; GGUF (llama.cpp's format) gets teal.
    var color: Color { self == .mlx ? .silver : .teal }
}

extension Color {
    /// Brushed-metal grey that stays readable in light and dark mode.
    static let silver = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor(calibratedWhite: 0.82, alpha: 1)
            : NSColor(calibratedWhite: 0.42, alpha: 1)
    })
}

/// A small tinted capsule: tags, capabilities, formats.
struct Chip: View {
    let label: String
    let systemImage: String
    let color: Color
    var compact = false

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: systemImage).font(.system(size: compact ? 8 : 9, weight: .semibold))
            Text(label).font(.system(size: compact ? 10 : 11, weight: .medium))
        }
        .foregroundStyle(color)
        .padding(.horizontal, compact ? 6 : 7)
        .padding(.vertical, compact ? 2 : 3)
        .background(color.opacity(0.14), in: Capsule())
    }
}

/// The maker's logo, or a coloured monogram when there's none.
struct ModelIcon: View {
    let maker: String?
    let name: String
    var size: CGFloat = 26

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: size * 0.28, style: .continuous)
        Group {
            if let image = IconStore.image(for: maker) {
                Image(nsImage: image)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fill)
                    .background(.white)
            } else {
                ZStack {
                    shape.fill(monogramColor.gradient)
                    Text(ModelIdentity.prettyName(name).prefix(1))
                        .font(.system(size: size * 0.46, weight: .bold, design: .rounded))
                        .foregroundStyle(.white)
                }
            }
        }
        .frame(width: size, height: size)
        .clipShape(shape)
        .overlay(shape.strokeBorder(.primary.opacity(0.08), lineWidth: 0.5))
    }

    private var monogramColor: Color {
        let palette: [Color] = [.blue, .purple, .pink, .orange, .teal, .indigo, .green]
        let key = ModelIdentity.prettyName(name).prefix(3)
        let hash = key.unicodeScalars.reduce(0) { $0 &+ Int($1.value) }
        return palette[hash % palette.count]
    }
}

@MainActor
enum IconStore {
    private static var cache: [String: NSImage] = [:]

    static func image(for maker: String?) -> NSImage? {
        guard let maker else { return nil }
        if let cached = cache[maker] { return cached }
        guard let url = Bundle.main.url(forResource: maker, withExtension: "png", subdirectory: "icons"),
              let image = NSImage(contentsOf: url) else { return nil }
        cache[maker] = image
        return image
    }
}
