import SwiftUI

struct InstalledModelRow: View {
    @Environment(ModelStore.self) private var store
    let model: LocalModel
    let isLoading: Bool

    var body: some View {
        ModelRowLayout(name: model.name, family: model.details?.family, subtitle: isLoading ? "Loading…" : subtitle, isLoaded: false,
                       capabilities: Capability.from(model.capabilities), isMLX: model.isMLX, diskSize: model.size) {
            if isLoading {
                ProgressView().controlSize(.small)
            } else {
                HeartButton(isFavorite: store.favorites.contains(model.name)) {
                    withAnimation(.snappy(duration: 0.25)) { store.toggleFavorite(model.name) }
                }
                RowButton(title: "Load", systemImage: nil) {
                    Task { await store.load(model.name) }
                }
                .help("Load into memory")
            }
        }
    }

    private var subtitle: String {
        var parts = [Format.bytes(model.size)]
        if let size = model.details?.parameterSize { parts.append(size) }
        return parts.joined(separator: " · ")
    }
}

/// An icon row like the Wi-Fi menu: the maker's icon, the name (click to rename),
/// details with capability icons, tag chips, and a "…" menu that appears on hover.
private struct ModelRowLayout<Actions: View>: View {
    @Environment(ModelStore.self) private var store
    @Environment(\.openWindow) private var openWindow
    let name: String
    let family: String?
    let subtitle: String
    let isLoaded: Bool
    let capabilities: [Capability]
    let isMLX: Bool
    let diskSize: Int64
    var context: ContextInfo? = nil
    @ViewBuilder let actions: () -> Actions
    @State private var confirmingDelete = false

    @State private var hovering = false
    @State private var isRenaming = false

    var body: some View {
        HStack(alignment: .center, spacing: 10) {
            ModelIcon(maker: ModelIdentity.maker(for: name, family: family), name: name, size: 28)
                .overlay(alignment: .bottomTrailing) {
                    if isLoaded {
                        Circle()
                            .fill(context?.activity.color.map(Color.init(nsColor:)) ?? .green)
                            .frame(width: 9, height: 9)
                            .overlay(Circle().strokeBorder(.background, lineWidth: 1.5))
                            .offset(x: 3, y: 3)
                    }
                }

            VStack(alignment: .leading, spacing: 2) {
                ModelTitle(name: name, isRenaming: $isRenaming)
                HStack(spacing: 5) {
                    Text(confirmingDelete ? "Delete to free \(Format.bytes(diskSize))?" : subtitle)
                        .foregroundStyle(confirmingDelete ? AnyShapeStyle(.red) : AnyShapeStyle(.secondary))
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .monospacedDigit()
                        .layoutPriority(-1)
                    ModelIconsLine(capabilities: capabilities, tags: store.tags(for: name), isMLX: isMLX)
                }
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
            }

            Spacer(minLength: 6)

            HStack(spacing: 6) {
                if store.pending[name] == .deleting {
                    ProgressView().controlSize(.small)
                } else if confirmingDelete {
                    // Inline, so the menu stays open while you decide.
                    RowButton(title: "Cancel", systemImage: nil) { confirmingDelete = false }
                    RowButton(title: "Delete", systemImage: "trash", isDestructive: true) {
                        confirmingDelete = false
                        Task { await store.delete(name) }
                    }
                    .help("Remove from disk, freeing \(Format.bytes(diskSize))")
                } else {
                    ModelMenu(name: name, onRename: { isRenaming = true }, onDelete: { confirmingDelete = true })
                        .opacity(hovering ? 1 : 0)
                    actions()
                }
            }
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 5)
        .background(hovering ? AnyShapeStyle(.quaternary.opacity(0.7)) : AnyShapeStyle(.clear), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .onHover { hovering = $0 }
    }

}

/// The format (GGUF or MLX) | capability icons | the icons of the model's
/// tags. Hovering an icon shows its name right away.
struct ModelIconsLine: View {
    let capabilities: [Capability]
    let tags: [ModelTag]
    var isMLX = false

    var body: some View {
        HStack(spacing: 5) {
            let format = isMLX ? ModelFormat.mlx : .gguf
            Text(format.label)
                .font(.system(size: 8, weight: .bold))
                .foregroundStyle(format.color)
                .instantTip(isMLX ? "MLX build, Apple's framework" : "GGUF build, for llama.cpp")
            if !capabilities.isEmpty {
                Separator()
                ForEach(capabilities) { capability in
                    Image(systemName: capability.systemImage)
                        .foregroundStyle(capability.color)
                        .instantTip(capability.label)
                }
            }
            if !tags.isEmpty {
                Separator()
                ForEach(tags) { tag in
                    Image(systemName: tag.systemImage)
                        .foregroundStyle(tag.color)
                        .instantTip("Used for \(tag.label.lowercased())")
                }
            }
        }
        .font(.system(size: 9, weight: .semibold))
        .fixedSize()
    }

    private struct Separator: View {
        var body: some View {
            Rectangle().fill(.tertiary).frame(width: 1, height: 9)
        }
    }
}

extension View {
    /// A label shown above the view as soon as the pointer is on it. The
    /// system tooltip (`.help`) waits about a second.
    func instantTip(_ text: String) -> some View { modifier(InstantTip(text: text)) }
}

private struct InstantTip: ViewModifier {
    let text: String
    @State private var hovering = false

    func body(content: Content) -> some View {
        content
            .scaleEffect(hovering ? 1.25 : 1)
            .brightness(hovering ? 0.15 : 0)
            .animation(.snappy(duration: 0.12), value: hovering)
            .onHover { hovering = $0 }
            .overlay(alignment: .top) {
                if hovering {
                    Text(text)
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(.white)
                        .fixedSize()
                        .padding(.horizontal, 7)
                        .padding(.vertical, 3)
                        .background(Color(white: 0.12), in: Capsule())
                        .overlay(Capsule().strokeBorder(.white.opacity(0.18), lineWidth: 0.5))
                        .shadow(color: .black.opacity(0.4), radius: 4, y: 1)
                        .offset(y: -20)
                        .allowsHitTesting(false)
                        .transition(.opacity)
                }
            }
            .zIndex(hovering ? 1 : 0)
    }
}

/// A model's name with its default star. Click it to rename.
struct ModelTitle: View {
    @Environment(ModelStore.self) private var store
    let name: String
    @Binding var isRenaming: Bool
    @State private var draft = ""
    @FocusState private var fieldFocused: Bool

    var body: some View {
        if isRenaming {
            TextField(ModelIdentity.prettyName(name), text: $draft)
                .textFieldStyle(.plain)
                .font(.system(size: 13))
                .focused($fieldFocused)
                .onSubmit(commit)
                .onExitCommand { isRenaming = false }
                .onChange(of: fieldFocused) { _, focused in
                    if !focused && isRenaming { commit() }
                }
                .onAppear {
                    draft = store.aliases[name] ?? ""
                    fieldFocused = true
                }
        } else {
            HStack(spacing: 4) {
                Text(store.aliases[name] ?? ModelIdentity.prettyName(name))
                    .font(.system(size: 13))
                    .lineLimit(1)
                    .truncationMode(.middle)
                if store.isDefault(name) {
                    Image(systemName: "star.fill")
                        .font(.system(size: 9))
                        .foregroundStyle(.yellow)
                        .help("Default model")
                }
            }
            .contentShape(Rectangle())
            .onTapGesture { isRenaming = true }
            .help(store.aliases[name] == nil ? "\(name)\nClick to rename" : "Ollama name: \(name)\nClick to rename")
        }
    }

    private func commit() {
        store.rename(name, to: draft)
        isRenaming = false
    }
}

/// The "…" menu: rename, tags, favorites, default, settings, delete.
struct ModelMenu: View {
    @Environment(ModelStore.self) private var store
    @Environment(\.openWindow) private var openWindow
    let name: String
    let onRename: () -> Void
    let onDelete: () -> Void

    var body: some View {
        Menu {
            Button("Rename…", action: onRename)
            if store.aliases[name] != nil {
                Button("Use Original Name") { store.rename(name, to: "") }
            }
            Button("Model Settings…") {
                openWindow(id: "model", value: name)
                NSApp.activate()
            }
            Button("View Log Stream") {
                openWindow(id: "logs")
                NSApp.activate()
            }
            Divider()
            Section("Used For") {
                ForEach(ModelTag.allCases) { tag in
                    Toggle(isOn: Binding(
                        get: { store.tags(for: name).contains(tag) },
                        set: { _ in store.toggle(tag, for: name) }
                    )) {
                        Label(tag.label, systemImage: tag.systemImage)
                    }
                }
            }
            Divider()
            Button(store.favorites.contains(name) ? "Remove from Favorites" : "Add to Favorites") {
                withAnimation(.snappy(duration: 0.25)) { store.toggleFavorite(name) }
            }
            if store.isDefault(name) {
                Button("Remove as Default") { store.defaultModel = nil }
            } else {
                Button("Set as Default") { store.defaultModel = name }
            }
            Button("Copy Ollama Name") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(name, forType: .string)
            }
            Divider()
            Button("Delete…", role: .destructive, action: onDelete)
        } label: {
            Image(systemName: "ellipsis.circle")
                .font(.system(size: 14))
                .foregroundStyle(.secondary)
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .help("Rename, tag and more")
    }
}

struct ContextInfo {
    let size: Int?
    let usage: ContextUsage?
    var activity: ModelActivity = .idle
}

/// One loaded model in the Memory card:
///   icon + name ............................ what it's doing
///   [context bar, full width                              ]
///   1K / 256K · 20 GB · 5 min left ............. 📌  Eject
struct LoadedModelCard: View {
    @Environment(ModelStore.self) private var store
    let model: RunningModel
    @State private var hovering = false
    @State private var isRenaming = false
    @State private var confirmingDelete = false

    var body: some View {
        let usage = store.contextUsage[model.name]
        let activity = store.activity[model.name] ?? .idle
        VStack(alignment: .leading, spacing: 7) {
            HStack(spacing: 8) {
                ModelIcon(maker: ModelIdentity.maker(for: model.name, family: model.details?.family), name: model.name, size: 22)
                    .overlay(alignment: .bottomTrailing) {
                        Circle()
                            .fill(activity.color.map(Color.init(nsColor:)) ?? .secondary)
                            .frame(width: 8, height: 8)
                            .overlay(Circle().strokeBorder(.background, lineWidth: 1.5))
                            .offset(x: 3, y: 3)
                    }
                VStack(alignment: .leading, spacing: 2) {
                    ModelTitle(name: model.name, isRenaming: $isRenaming)
                    ModelIconsLine(capabilities: store.capabilities(for: model.name), tags: store.tags(for: model.name),
                                   isMLX: model.details?.format == "safetensors")
                }
                Spacer(minLength: 8)
                ActivityLabel(activity: activity)
            }

            GeometryReader { geometry in
                let fraction = usage?.fraction ?? 0
                ZStack(alignment: .leading) {
                    Capsule().fill(.quaternary)
                    Capsule().fill(fillColor(fraction).gradient)
                        .frame(width: fraction > 0 ? max(3, geometry.size.width * fraction) : 0)
                }
            }
            .frame(height: 5)
            .help(usage.map { "Context: \($0.used.formatted()) of \($0.size.formatted()) tokens (\(Int($0.fraction * 100))%)." }
                  ?? "Context fill isn't available for MLX models.")

            HStack(spacing: 6) {
                if confirmingDelete {
                    Text("Delete to free \(Format.bytes(store.installed.first { $0.name == model.name }?.size ?? model.size))?")
                        .foregroundStyle(.red)
                } else {
                    Text(status(usage))
                        .monospacedDigit()
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                Spacer(minLength: 6)
                HStack(spacing: 6) { actions }
                    .fixedSize()
            }
            .font(.system(size: 11))
            .foregroundStyle(.secondary)
        }
        .padding(.vertical, 4)
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
    }

    @ViewBuilder private var actions: some View {
        if store.pending[model.name] == .ejecting || store.pending[model.name] == .deleting {
            ProgressView().controlSize(.small)
        } else if confirmingDelete {
            RowButton(title: "Cancel", systemImage: nil) { confirmingDelete = false }
            RowButton(title: "Delete", systemImage: "trash", isDestructive: true) {
                confirmingDelete = false
                Task { await store.delete(model.name) }
            }
        } else {
            ModelMenu(name: model.name, onRename: { isRenaming = true }, onDelete: { confirmingDelete = true })
                .opacity(hovering ? 1 : 0)
            let keptAlive = store.isKeptAlive(model)
            RowButton(title: nil, systemImage: keptAlive ? "pin.fill" : "pin", isActive: keptAlive) {
                Task { await store.setKeepAlive(!keptAlive, for: model.name) }
            }
            .help(keptAlive ? "Keep Alive is on: stays loaded until you eject it" : "Keep Alive: stay loaded until you eject it")
            RowButton(title: "Eject", systemImage: "eject.fill", hoverDestructive: true) {
                Task { await store.eject(model.name) }
            }
            .help("Eject from memory")
        }
    }

    /// "1K / 256K · 20 GB · 5 min left"
    private func status(_ usage: ContextUsage?) -> String {
        var parts: [String] = []
        if let usage {
            parts.append("\(Format.tokens(usage.used)) / \(Format.tokens(usage.size))")
        } else if let size = model.contextLength {
            parts.append("\(Format.tokens(size)) context")
        }
        parts.append(Format.compact(model.size))
        if store.isKeptAlive(model) {
            parts.append("Kept alive")
        } else if model.staysLoaded {
            parts.append("Stays loaded") // another app asked Ollama to keep it
        } else if let expiresAt = model.expiresAt {
            parts.append(Format.remaining(until: expiresAt))
        }
        return parts.joined(separator: " · ")
    }

    private func fillColor(_ fraction: Double) -> Color {
        switch fraction {
        case ..<0.6: .green
        case ..<0.85: .yellow
        default: .red
        }
    }
}

/// "Idle", "Reading 34%" or "Generating · 69 tok/s", with a pulsing dot while busy.
private struct ActivityLabel: View {
    let activity: ModelActivity

    var body: some View {
        HStack(spacing: 5) {
            switch activity {
            case .idle:
                Text("Idle").foregroundStyle(.tertiary)
            case .reading(let done, let total):
                PulsingDot(color: .orange)
                Text("Reading \(total > 0 ? done * 100 / total : 0)%")
                    .foregroundStyle(.orange)
                    .help("Processing the prompt: \(done.formatted()) of \(total.formatted()) tokens. Long chats make this slow.")
            case .writing(let tokens, let perSecond):
                PulsingDot(color: .green)
                Text(perSecond.map { "Generating · \(Int($0.rounded())) tok/s" } ?? "Generating")
                    .foregroundStyle(.green)
                    .help("\(tokens.formatted()) tokens so far. Thinking and answering look the same from here.")
            }
        }
        .font(.system(size: 11, weight: .medium))
        .monospacedDigit()
        .lineLimit(1)
        .fixedSize()
    }
}

struct PulsingDot: View {
    let color: Color
    @State private var on = false

    var body: some View {
        Circle()
            .fill(color)
            .frame(width: 6, height: 6)
            .opacity(on ? 1 : 0.35)
            .animation(.easeInOut(duration: 0.7).repeatForever(autoreverses: true), value: on)
            .onAppear { on = true }
    }
}

/// Moves a model into (or out of) Favorites.
private struct HeartButton: View {
    let isFavorite: Bool
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            Image(systemName: isFavorite ? "heart.fill" : "heart")
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(isFavorite ? AnyShapeStyle(.pink) : hovering ? AnyShapeStyle(.primary) : AnyShapeStyle(.secondary))
                .contentTransition(.symbolEffect(.replace))
                .frame(width: 20, height: 20)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .pointerStyle(.link)
        .onHover { hovering = $0 }
        .help(isFavorite ? "Remove from Favorites" : "Add to Favorites")
    }
}

/// A small capsule button, like the ones in System Settings lists.
private struct RowButton: View {
    let title: String?
    let systemImage: String?
    var isActive = false
    var isDestructive = false
    /// Turns red while hovered, for actions like Eject.
    var hoverDestructive = false
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 4) {
                if let systemImage {
                    Image(systemName: systemImage).font(.system(size: 9, weight: .semibold))
                }
                if let title {
                    Text(title).font(.system(size: 11, weight: .medium))
                }
            }
            .foregroundStyle(isActive || isDestructive || (hoverDestructive && hovering) ? AnyShapeStyle(.white) : AnyShapeStyle(.primary))
            .padding(.horizontal, title == nil ? 7 : 10)
            .padding(.vertical, 3)
            .frame(minHeight: 18)
            .background(isDestructive || (hoverDestructive && hovering) ? AnyShapeStyle(Color.red)
                        : isActive ? AnyShapeStyle(Color.accentColor) : AnyShapeStyle(.quaternary), in: Capsule())
            .animation(.easeOut(duration: 0.12), value: hovering)
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .pointerStyle(.link)
        .onHover { hovering = $0 }
    }
}

/// Where models live, and how the drive splits into models, other files and free space.
struct StorageCard: View {
    let storage: StorageInfo
    let models: Int64

    private var other: Int64 { max(0, storage.total - storage.free - models) }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                Image(systemName: storage.isExternal ? "externaldrive.fill" : "internaldrive.fill")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                Text(storage.path)
                    .font(.system(size: 11))
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(storage.linkedFrom.map { "\($0) links here" } ?? storage.path)
                Spacer(minLength: 4)
                Button {
                    NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: storage.path)])
                } label: {
                    Image(systemName: "arrow.up.forward.app")
                        .font(.system(size: 11))
                }
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
                .help("Show in Finder")
            }

            GeometryReader { geometry in
                let total = max(1, Double(storage.total))
                HStack(spacing: 2) {
                    Capsule().fill(Color.accentColor)
                        .frame(width: max(4, geometry.size.width * Double(models) / total))
                    Capsule().fill(Color.secondary.opacity(0.45))
                        .frame(width: max(4, geometry.size.width * Double(other) / total))
                    Capsule().fill(.quaternary)
                }
            }
            .frame(height: 6)

            HStack(spacing: 12) {
                Legend(color: .accentColor, label: "Models", value: models)
                Legend(color: .secondary.opacity(0.45), label: "Other", value: other)
                Legend(color: Color.secondary.opacity(0.2), label: "Free", value: storage.free)
                Spacer(minLength: 0)
                if let volume = storage.volumeName {
                    Text(volume)
                        .font(.system(size: 10))
                        .foregroundStyle(.tertiary)
                }
            }
        }
        .padding(12)
        .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}

private struct Legend: View {
    let color: Color
    let label: String
    let value: Int64

    var body: some View {
        HStack(spacing: 4) {
            Circle().fill(color).frame(width: 6, height: 6)
            Text(label).foregroundStyle(.secondary)
            Text(Format.compact(value)).fontWeight(.medium).monospacedDigit()
        }
        .font(.system(size: 10))
        .fixedSize()
    }
}

/// Memory used by loaded models against the Mac's RAM, as a card. A model
/// being loaded shows here too, and its share of the bar fills as it loads.
struct MemoryCard: View {
    @Environment(ModelStore.self) private var store
    let used: Int64
    let total: Int64

    var body: some View {
        TimelineView(.periodic(from: .now, by: 0.25)) { context in
            let loading = store.loadingModels
            let loadingSize = loading.reduce(0.0) { $0 + Double($1.size) }
            let loadingBytes = loading.reduce(0.0) { $0 + Double($1.size) * store.loadProgress($1, at: context.date) }
            VStack(alignment: .leading, spacing: 8) {
                // While loading, the card tracks the load: how much of the model is in
                // memory so far, out of its size. Otherwise it's the whole Mac's memory,
                // split into loaded models, everything else, and free.
                if loading.isEmpty {
                    let systemUsed = max(used, store.systemMemoryUsed ?? used)
                    let other = systemUsed - used
                    HStack(alignment: .firstTextBaseline, spacing: 4) {
                        Text("Memory")
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(.secondary)
                        Spacer()
                        Text(Format.bytes(systemUsed))
                            .font(.system(size: 13, weight: .semibold))
                            .monospacedDigit()
                            .foregroundStyle(store.memoryPressure.isHigh ? AnyShapeStyle(.red) : AnyShapeStyle(.primary))
                            .help(store.memoryPressure.help)
                        Text("used of \(Format.ram(total))")
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                    }
                    // Models blue, other apps orange, free grey.
                    GeometryReader { geometry in
                        let width = geometry.size.width
                        HStack(spacing: 2) {
                            Capsule().fill(Color.accentColor.gradient)
                                .frame(width: max(4, width * fraction(Double(used))))
                            Capsule().fill(Color.orange.gradient)
                                .frame(width: max(4, width * fraction(Double(other))))
                            Capsule().fill(.quaternary)
                        }
                    }
                    .frame(height: 6)
                    HStack(spacing: 12) {
                        Legend(color: .accentColor, label: "Models", value: used)
                        Legend(color: .orange, label: "Other", value: other)
                        Legend(color: Color.secondary.opacity(0.35), label: "Free", value: max(0, total - systemUsed))
                        Spacer(minLength: 0)
                    }
                } else {
                    HStack(alignment: .firstTextBaseline, spacing: 4) {
                        Text("Loading into Memory")
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(.secondary)
                        Spacer()
                        Text(Format.bytes(Int64(loadingBytes)))
                            .font(.system(size: 13, weight: .semibold))
                            .monospacedDigit()
                        Text("of \(Format.bytes(Int64(loadingSize)))")
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                    }
                    GeometryReader { geometry in
                        let filled = loadingSize > 0 ? loadingBytes / loadingSize : 0
                        ZStack(alignment: .leading) {
                            Capsule().fill(.quaternary)
                            Capsule()
                                .fill(tint.gradient)
                                .frame(width: filled > 0 ? max(6, geometry.size.width * filled) : 0)
                        }
                    }
                    .frame(height: 6)
                }

                // Each loaded model once: its row (name, Keep Alive, Eject, "…"),
                // then its context fill and what it's doing.
                if !store.loaded.isEmpty {
                    Divider().padding(.vertical, 2)
                    ForEach(store.loaded) { model in
                        LoadedModelCard(model: model)
                    }
                }

                ForEach(loading) { model in
                    HStack(spacing: 8) {
                        ModelIcon(maker: ModelIdentity.maker(for: model.name, family: model.details?.family), name: model.name, size: 18)
                        Text(store.aliases[model.name] ?? ModelIdentity.prettyName(model.name))
                            .font(.system(size: 12))
                            .lineLimit(1)
                        Spacer(minLength: 4)
                        Text("Loading \(Int(store.loadProgress(model, at: context.date) * 100))%")
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                    }
                    .transition(.opacity)
                }
            }
        }
        .padding(12)
        .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .animation(.smooth, value: store.loadingModels.map(\.name))
    }

    private func fraction(_ bytes: Double) -> Double {
        guard total > 0 else { return 0 }
        return min(1, bytes / Double(total))
    }

    private var tint: Color {
        switch fraction(Double(used)) {
        case ..<0.6: .accentColor
        case ..<0.8: .orange
        default: .red
        }
    }
}

enum Format {
    /// Decimal units, matching `ollama list`.
    static func bytes(_ value: Int64) -> String {
        if value == 0 { return "0 GB" }
        return ByteCountFormatter.string(fromByteCount: value, countStyle: .file)
    }

    /// Short sizes for tight spaces: "119 GB", "8.2 GB", "1.5 TB".
    static func compact(_ value: Int64) -> String {
        let gb = Double(value) / 1e9
        if gb >= 1000 { return String(format: "%.1f TB", gb / 1000) }
        if gb >= 10 { return String(format: "%.0f GB", gb) }
        return String(format: "%.1f GB", gb)
    }

    /// "<1m", "12m", "1h", "3h 20m", "2d".
    static func uptime(since date: Date) -> String {
        let minutes = Int(Date.now.timeIntervalSince(date) / 60)
        if minutes < 1 { return "<1m" }
        if minutes < 60 { return "\(minutes)m" }
        let hours = minutes / 60
        if hours < 24 { return minutes % 60 == 0 || hours >= 10 ? "\(hours)h" : "\(hours)h \(minutes % 60)m" }
        return "\(hours / 24)d"
    }

    /// Physical RAM in binary units, so a 64 GB Mac reads "64 GB".
    static func ram(_ value: Int64) -> String {
        ByteCountFormatter.string(fromByteCount: value, countStyle: .memory)
    }

    static func tokens(_ value: Int) -> String {
        value >= 1024 ? "\(value / 1024)K" : "\(value)"
    }

    /// Time until Ollama auto-ejects the model, e.g. "12 min left".
    static func remaining(until date: Date) -> String {
        if date.timeIntervalSinceNow > 365 * 24 * 3600 { return "Kept alive" }
        let minutes = max(0, Int((date.timeIntervalSinceNow / 60).rounded(.up)))
        if minutes < 60 { return "\(minutes) min left" }
        let rest = minutes % 60
        return rest == 0 ? "\(minutes / 60) h left" : "\(minutes / 60) h \(rest) min left"
    }
}
