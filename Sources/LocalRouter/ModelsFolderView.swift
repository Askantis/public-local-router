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

/// Where speech models live (PROTOCOL.md 2): the Askantis models folder and
/// the Hugging Face cache, both always searched. Chooses where new ones go and
/// moves models between the two.
struct SpeechModelPlacesView: View {
    /// In the welcome guide the service may not run yet: then show nothing.
    var hidesWhenStopped = false
    @State private var daemon = DaemonStore.shared
    @State private var move: (from: ModelPlacesInfo.Folder, to: ModelPlacesInfo.Folder)?

    var body: some View {
        if daemon.running, let info = daemon.modelPlaces {
            VStack(alignment: .leading, spacing: 12) {
                Picker("Where new models go", selection: Binding(get: { info.target }, set: { choose($0, info) })) {
                    Text("Hugging Face folder").tag("huggingface")
                    Text("Askantis folder").tag("askantis")
                    Divider()
                    Text(info.target == "custom" ? URL(fileURLWithPath: info.path).lastPathComponent : "Other folder…").tag("custom")
                }
                .disabled(isMoving)

                ForEach(info.folders) { folder in
                    folderRow(folder, info: info)
                }
                if info.target == "custom" {
                    customRow(info)
                }
                status
            }
            .confirmationDialog(moveTitle, isPresented: Binding(get: { move != nil }, set: { if !$0 { move = nil } }),
                                titleVisibility: .visible) {
                Button("Move") {
                    guard let move else { return }
                    Task { await daemon.moveSpeechModels(from: move.from, to: move.to) }
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("Apps keep finding them in either folder. Hugging Face tools, like Python's huggingface_hub, only look in the Hugging Face folder.")
            }
        } else if daemon.running {
            ProgressView().controlSize(.small)
                .task { await daemon.loadModelPlaces() }
        } else if !hidesWhenStopped {
            Label("Start Local Router's background service (Settings → Apps) to see where speech models are.", systemImage: "info.circle")
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
        }
    }

    private func folderRow(_ folder: ModelPlacesInfo.Folder, info: ModelPlacesInfo) -> some View {
        let other = info.folders.first { $0.place != folder.place }
        let canMoveHere = other.map { $0.models > 0 && $0.state == "ok" } ?? false && folder.state == "ok"
        return HStack(spacing: 12) {
            Image(systemName: folder.path.hasPrefix("/Volumes/") ? "externaldrive.fill" : "internaldrive.fill")
                .font(.system(size: 18))
                .foregroundStyle(.secondary)
                .frame(width: 28)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(folder.title).font(.system(size: 13, weight: .semibold))
                    if info.target == folder.place {
                        Text("New models go here")
                            .font(.system(size: 10, weight: .medium))
                            .padding(.horizontal, 6).padding(.vertical, 1)
                            .background(Color.accentColor.opacity(0.15), in: Capsule())
                    }
                }
                Text(folder.path)
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(folder.path)
                Text(details(folder))
                    .font(.system(size: 11))
                    .foregroundStyle(folder.state == "ok" ? Color.secondary : Color.orange)
            }
            Spacer()
            Button("Show in Finder") {
                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: folder.path)])
            }
            .disabled(!FileManager.default.fileExists(atPath: folder.path))
            if let other {
                Button("Move Here…") { move = (other, folder) }
                    .disabled(!canMoveHere || isMoving)
                    .help("Move the models in the \(other.title) here")
            }
        }
        .padding(10)
        .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }

    private func customRow(_ info: ModelPlacesInfo) -> some View {
        HStack(spacing: 12) {
            Image(systemName: "folder.fill").font(.system(size: 18)).foregroundStyle(.secondary).frame(width: 28)
            VStack(alignment: .leading, spacing: 2) {
                Text("Other folder").font(.system(size: 13, weight: .semibold))
                Text(info.path).font(.system(size: 11)).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                Text("New models go here. Other Askantis apps only look in the two folders above.")
                    .font(.system(size: 11)).foregroundStyle(.secondary)
            }
            Spacer()
            Button("Change…") { chooseFolder(info) }
        }
        .padding(10)
        .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }

    @ViewBuilder private var status: some View {
        switch daemon.placesMove {
        case .idle:
            Text("Speech models for apps like Scribely are found in both folders and never downloaded twice.")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        case .working(let step, let fraction):
            VStack(alignment: .leading, spacing: 6) {
                if let fraction { ProgressView(value: fraction) } else { ProgressView().progressViewStyle(.linear) }
                Text(fraction.map { "\(step) \(Int($0 * 100))%" } ?? step)
                    .font(.system(size: 11)).foregroundStyle(.secondary).monospacedDigit()
            }
        case .finished(let message):
            Label(message, systemImage: "checkmark.circle.fill").font(.system(size: 12)).foregroundStyle(.green)
        case .failed(let message):
            Label(message, systemImage: "exclamationmark.triangle.fill")
                .font(.system(size: 12)).foregroundStyle(.orange)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var isMoving: Bool {
        if case .working = daemon.placesMove { return true }
        return false
    }

    private var moveTitle: String {
        guard let move else { return "" }
        let count = move.from.models == 1 ? "1 model" : "\(move.from.models) models"
        return "Move \(count) (\(Format.bytes(move.from.bytes))) to the \(move.to.title)?"
    }

    private func details(_ folder: ModelPlacesInfo.Folder) -> String {
        switch folder.state {
        case "ok":
            if !FileManager.default.fileExists(atPath: folder.path) { return "Not created yet" }
            return folder.models == 1 ? "1 model · \(Format.bytes(folder.bytes))" : "\(folder.models) models · \(Format.bytes(folder.bytes))"
        case "checking": return "Checking…"
        case "missing_volume": return "Drive not connected"
        case "not_allowed": return "Not allowed: Privacy & Security → Files & Folders"
        case "unwritable": return "Can't write to the folder"
        default: return "Unknown"
        }
    }

    private func choose(_ place: String, _ info: ModelPlacesInfo) {
        if place == "custom" {
            chooseFolder(info)
        } else if place != info.target {
            Task { await daemon.setDownloadTarget(place) }
        }
    }

    private func chooseFolder(_ info: ModelPlacesInfo) {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = "Use Folder"
        panel.message = "Choose where Local Router should put new speech models."
        if let custom = info.customPath { panel.directoryURL = URL(fileURLWithPath: custom) }
        NSApp.activate()
        guard panel.runModal() == .OK, let folder = panel.url else { return }
        Task { await daemon.setDownloadTarget("custom", path: folder.standardizedFileURL.path) }
    }
}
