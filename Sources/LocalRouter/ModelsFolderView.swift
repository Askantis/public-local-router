import AppKit
import SwiftUI

/// Where models are downloaded, with a Change… button. Used in its own
/// window (from Settings) and in onboarding.
struct ModelsFolderView: View {
    @Environment(ModelStore.self) private var store
    @State private var plan: ModelStore.FolderChangePlan?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            if let storage = store.storage {
                HStack(spacing: 12) {
                    Image(systemName: storage.isExternal ? "externaldrive.fill" : "internaldrive.fill")
                        .font(.system(size: 22))
                        .foregroundStyle(.secondary)
                        .frame(width: 32)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(storage.path)
                            .font(.system(size: 13, weight: .semibold))
                            .lineLimit(1)
                            .truncationMode(.middle)
                        Text(details(storage))
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: storage.path)])
                    }
                    Button("Change…", action: chooseFolder)
                        .buttonStyle(.borderedProminent)
                        .disabled(isWorking)
                }
                .padding(12)
                .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            } else {
                Label("Start Ollama to see where models are stored.", systemImage: "info.circle")
                    .font(.system(size: 12))
                    .foregroundStyle(.secondary)
            }

            status
        }
        .confirmationDialog(dialogTitle, isPresented: Binding(get: { plan != nil }, set: { if !$0 { plan = nil } }),
                            titleVisibility: .visible, presenting: plan) { plan in
            if plan.folderHasModels {
                Button("Use This Folder") { run(plan, move: false) }
            } else if plan.modelCount > 0 {
                Button("Move \(plan.modelCount) Models") { run(plan, move: true) }
                    .disabled(!plan.enoughSpace)
                Button("Switch Without Moving") { run(plan, move: false) }
            } else {
                Button("Use This Folder") { run(plan, move: false) }
            }
            Button("Cancel", role: .cancel) {}
        } message: { plan in
            Text(dialogMessage(plan))
        }
    }

    @ViewBuilder private var status: some View {
        switch store.folderChange {
        case .idle:
            HStack(alignment: .firstTextBaseline, spacing: 4) {
                Text("New downloads go here, for the Ollama app, Homebrew and `ollama serve` alike.")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                InfoButton()
            }
        case .working(let step, let fraction):
            VStack(alignment: .leading, spacing: 6) {
                if let fraction {
                    ProgressView(value: fraction)
                } else {
                    ProgressView().progressViewStyle(.linear)
                }
                Text(fraction.map { "\(step) \(Int($0 * 100))%" } ?? step)
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
        case .finished(let message):
            Label(message, systemImage: "checkmark.circle.fill")
                .font(.system(size: 12))
                .foregroundStyle(.green)
        case .failed(let message):
            Label(message, systemImage: "exclamationmark.triangle.fill")
                .font(.system(size: 12))
                .foregroundStyle(.orange)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var isWorking: Bool {
        if case .working = store.folderChange { return true }
        return false
    }

    private var dialogTitle: String {
        guard let plan else { return "" }
        if plan.folderHasModels { return "Use the models in “\(plan.folder.lastPathComponent)”?" }
        return plan.modelCount > 0 ? "Move your models to “\(plan.folder.lastPathComponent)”?" : "Download models to “\(plan.folder.lastPathComponent)”?"
    }

    private func dialogMessage(_ plan: ModelStore.FolderChangePlan) -> String {
        if plan.folderHasModels {
            return "This folder already has Ollama models. Ollama will use them instead of the ones in the current folder, which stay where they are."
        }
        guard plan.modelCount > 0 else { return "Ollama restarts briefly to pick up the change." }
        var text = "Ollama stops while \(plan.modelCount) models (\(Format.bytes(plan.bytes))) are moved, then starts again. "
        text += plan.enoughSpace
            ? "Originals are deleted only after every file is copied and checked."
            : "There isn't enough free space in that folder to move them."
        text += " Switching without moving leaves them in the old folder, where Ollama won't see them."
        return text
    }

    private func details(_ storage: StorageInfo) -> String {
        var parts = ["\(store.installed.count) models", Format.bytes(store.modelsDiskUsage)]
        if let volume = storage.volumeName { parts.append("\(Format.bytes(storage.free)) free on \(volume)") }
        return parts.joined(separator: " · ")
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = "Use Folder"
        panel.message = "Choose where Ollama should keep models."
        if let path = store.storage?.path { panel.directoryURL = URL(fileURLWithPath: path).deletingLastPathComponent() }
        NSApp.activate()
        guard panel.runModal() == .OK, let folder = panel.url else { return }
        if folder.standardizedFileURL.path == store.storage?.path { return }
        store.folderChange = .idle
        plan = store.plan(for: folder)
    }

    private func run(_ plan: ModelStore.FolderChangePlan, move: Bool) {
        Task { await store.changeModelsFolder(to: plan.folder, moveModels: move) }
    }
}

/// ⓘ with a short explanation of how the models folder is set.
private struct InfoButton: View {
    @State private var showing = false

    var body: some View {
        Button { showing.toggle() } label: {
            Image(systemName: "info.circle")
                .foregroundStyle(.secondary)
        }
        .buttonStyle(.plain)
        .pointerStyle(.link)
        .help("How this works")
        .popover(isPresented: $showing, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 10) {
                Text("How the Models Folder Works")
                    .font(.system(size: 13, weight: .semibold))
                Text("Ollama always looks for models in `~/.ollama/models`. Local Router turns that folder into a link, a symlink, that points to the folder you choose. Ollama follows the link, so your models can live anywhere, like on an external drive.")
                Text("**Why a link?** It works the same for the Ollama app, Homebrew and `ollama serve`, and it survives Ollama updates. The `OLLAMA_MODELS` setting only reaches one way of running Ollama, and it overrides the link, so Local Router tells you if it's set.")
                Text("**Moving is safe.** Every file is copied and checked before the originals are deleted.")
            }
            .font(.system(size: 12))
            .fixedSize(horizontal: false, vertical: true)
            .frame(width: 320)
            .padding(16)
        }
    }
}

/// The models-folder setting in its own window, opened from Settings.
struct ModelsFolderWindow: View {
    var body: some View {
        ModelsFolderView()
        .padding(20)
        .windowHeader("Models Folder")
        .frame(width: 560)
        .background(BringToFront())
    }
}
