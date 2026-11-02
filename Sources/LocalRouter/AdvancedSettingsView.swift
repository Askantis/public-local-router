import AppKit
import SwiftUI

/// Ollama's own server settings: the Advanced page in Settings.
struct AdvancedSettingsView: View {
    @Environment(ModelStore.self) private var store

    var body: some View {
        @Bindable var store = store
        Form {
            if !store.pendingSettingKeys.isEmpty || store.applyingSettings {
                Section {
                    HStack(spacing: 10) {
                        Image(systemName: "arrow.clockwise.circle.fill")
                            .font(.system(size: 18))
                            .foregroundStyle(.orange)
                        VStack(alignment: .leading, spacing: 2) {
                            Text("Restart Ollama to Apply")
                                .fontWeight(.semibold)
                            Text("Loaded models are ejected, and apps using Ollama wait a moment.")
                                .font(.system(size: 11))
                                .foregroundStyle(.secondary)
                        }
                        Spacer()
                        if store.applyingSettings {
                            ProgressView().controlSize(.small)
                        } else {
                            Button("Apply and Restart") { Task { await store.applyServerSettings() } }
                                .buttonStyle(.borderedProminent)
                        }
                    }
                }
            }

            Section {
                Picker("Default Context Length", selection: $store.serverSettingsValue.contextLength) {
                    Text("Automatic").tag(Int?.none)
                    ForEach(Self.contextSizes, id: \.self) { size in
                        Text(Format.tokens(size)).tag(Int?.some(size))
                    }
                }
            } header: {
                Text("Context")
            } footer: {
                Footnote("For apps that don't ask for a size. Bigger contexts use more memory. Set one model's size in its Model Settings.")
            }

            Section {
                Picker("Memory Saver", selection: $store.serverSettingsValue.kvCacheType) {
                    Text("Off").tag(String?.none)
                    Text("Balanced").tag(String?.some("q8_0"))
                    Text("Maximum").tag(String?.some("q4_0"))
                }
                Picker("Flash Attention", selection: $store.serverSettingsValue.flashAttention) {
                    Text("Automatic").tag(Bool?.none)
                    Text("On").tag(Bool?.some(true))
                    Text("Off").tag(Bool?.some(false))
                }
            } header: {
                Text("Memory")
            } footer: {
                Footnote(homebrewNote ?? "Memory Saver stores the context in less memory: Balanced about halves it with very little quality loss. It needs Flash Attention.")
            }

            Section {
                Picker("Models Loaded at Once", selection: $store.serverSettingsValue.maxLoadedModels) {
                    Text("Automatic").tag(Int?.none)
                    ForEach(1...6, id: \.self) { Text("\($0)").tag(Int?.some($0)) }
                }
                Picker("Parallel Requests per Model", selection: $store.serverSettingsValue.numParallel) {
                    Text("Automatic").tag(Int?.none)
                    ForEach([1, 2, 4, 8], id: \.self) { Text("\($0)").tag(Int?.some($0)) }
                }
                Picker("Unload Idle Models After", selection: $store.serverSettingsValue.keepAlive) {
                    Text("5 minutes (default)").tag(String?.none)
                    Text("15 minutes").tag(String?.some("15m"))
                    Text("1 hour").tag(String?.some("1h"))
                    Text("4 hours").tag(String?.some("4h"))
                    Text("Never").tag(String?.some("-1"))
                }
            } header: {
                Text("Loading")
            } footer: {
                Footnote("Each parallel request reserves its own context, so 4 means four times the context memory. Unloading applies to every app that uses Ollama.")
            }

            Section {
                LabeledContent("Ollama Is Running With") {
                    Text(runningSummary)
                        .multilineTextAlignment(.trailing)
                        .foregroundStyle(.secondary)
                }
                Button("Reset to Ollama's Defaults") {
                    store.serverSettingsValue = OllamaSettings()
                }
                .disabled(store.serverSettingsValue == OllamaSettings())
            } footer: {
                Footnote("Local Router saves these and sets them again at every login, because macOS forgets them on restart. Settings made in the Ollama app itself can take precedence.")
            }
        }
        .formStyle(.grouped)
        .onChange(of: store.serverSettingsValue) { _, value in value.save() }
        .task { await store.refreshRunningEnvironment() }
    }

    static let contextSizes = [4096, 8192, 16384, 32768, 65536, 131072, 262144]

    private var homebrewNote: String? {
        let keys = store.homebrewOverrides
        guard !keys.isEmpty, OllamaRunnerKind.current == .brewService else { return nil }
        let list = keys.map { "\($0.key)=\($0.value)" }.sorted().joined(separator: ", ")
        return "Your Homebrew Ollama service sets \(list) itself, which wins over these settings."
    }

    private var runningSummary: String {
        guard let running = store.runningEnvironment else { return "Ollama isn't running" }
        let shown = running.filter { OllamaSettings().environment.keys.contains($0.key) }
        if shown.isEmpty { return "Ollama's defaults" }
        return shown.map { "\($0.key.replacingOccurrences(of: "OLLAMA_", with: "")) \($0.value)" }.sorted().joined(separator: "\n")
    }
}

struct Footnote: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .foregroundStyle(.secondary)
            .multilineTextAlignment(.leading)
            .frame(maxWidth: .infinity, alignment: .leading)
            .fixedSize(horizontal: false, vertical: true)
    }
}

/// Which way Ollama runs, for notes that only apply to one of them.
enum OllamaRunnerKind {
    case app, brewService, serve, notRunning

    static var current: OllamaRunnerKind {
        switch OllamaRunner.detect() {
        case .app: .app
        case .brewService: .brewService
        case .serve: .serve
        case .notRunning: .notRunning
        }
    }
}

/// Settings for one model, from its "…" menu.
struct ModelSettingsView: View {
    @Environment(ModelStore.self) private var store
    let modelName: String

    /// "Ollama's default (256K)": the size it's loaded with now if we can
    /// see it, else the server's configured default.
    private var defaultLabel: String {
        let loaded = store.loaded.first { $0.name == modelName }?.contextLength
        let configured = store.serverSettings.contextLength
            ?? store.runningEnvironment?["OLLAMA_CONTEXT_LENGTH"].flatMap(Int.init)
        if let size = (store.modelContext[modelName] == nil ? loaded : nil) ?? configured {
            return "Ollama's default (\(Format.tokens(size)))"
        }
        return "Ollama's default (automatic)"
    }

    var body: some View {
        let model = store.installed.first { $0.name == modelName }
        let maximum = model?.details?.contextLength
        Form {
            Section {
                HStack(spacing: 12) {
                    ModelIcon(maker: ModelIdentity.maker(for: modelName, family: model?.details?.family), name: modelName, size: 40)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(store.aliases[modelName] ?? ModelIdentity.prettyName(modelName))
                            .font(.system(size: 15, weight: .semibold))
                        Text(modelName)
                            .font(.system(size: 11, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    }
                }
                if let model {
                    LabeledContent("Size", value: Format.bytes(model.size))
                    if let parameters = model.details?.parameterSize { LabeledContent("Parameters", value: parameters) }
                    if let quantization = model.details?.quantizationLevel { LabeledContent("Quantization", value: quantization) }
                    LabeledContent("Format", value: model.isMLX ? "MLX" : "GGUF")
                    if let maximum { LabeledContent("Maximum Context", value: Format.tokens(maximum)) }
                }
            }

            Section {
                Picker("Context Length", selection: Binding(
                    get: { store.modelContext[modelName] },
                    set: { store.setContextLength($0, for: modelName) }
                )) {
                    Text(defaultLabel).tag(Int?.none)
                    ForEach(AdvancedSettingsView.contextSizes.filter { $0 <= (maximum ?? .max) }, id: \.self) { size in
                        Text(Format.tokens(size)).tag(Int?.some(size))
                    }
                }
                if store.loaded.contains(where: { $0.name == modelName }) {
                    LabeledContent("Loaded Now") {
                        Button("Reload with This Setting") {
                            Task {
                                let wasPinned = store.pinned.contains(modelName)
                                await store.eject(modelName)
                                await store.load(modelName, manual: false)
                                if wasPinned { await store.setKeepAlive(true, for: modelName) }
                            }
                        }
                    }
                }
            } header: {
                Text("Context")
            } footer: {
                Footnote("Used whenever Local Router loads this model. Bigger contexts use more memory. Apps that ask Ollama for a different size make it reload the model.")
            }
        }
        .formStyle(.grouped)
        .windowHeader("Model Settings")
        .frame(width: 460)
        .task { await store.refreshRunningEnvironment() }
        .fixedSize(horizontal: false, vertical: true)
        .background(BringToFront())
    }
}
