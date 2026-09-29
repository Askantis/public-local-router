import SwiftUI

/// Laid out like the system Wi-Fi and Control Center menus: a header, a card,
/// icon rows with hover highlights, and three buttons at the bottom.
struct MenuContentView: View {
    @Environment(ModelStore.self) private var store
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismiss) private var dismiss
    @AppStorage("loadedCollapsed") private var loadedCollapsed = false
    @AppStorage("installedCollapsed") private var installedCollapsed = false
    @AppStorage(SettingsKey.favoritesCollapsed) private var favoritesCollapsed = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
                .padding(.horizontal, 14)
                .padding(.top, 12)
                .padding(.bottom, 10)

            switch store.status {
            case .checking:
                ProgressView()
                    .controlSize(.small)
                    .frame(maxWidth: .infinity)
                    .padding(28)
            case .offline:
                OfflineView()
            case .running:
                // Only while something is loaded or loading.
                if !store.loaded.isEmpty || !store.loadingModels.isEmpty {
                    MemoryCard(used: store.usedMemory, total: store.totalMemory)
                        .padding(.horizontal, 10)
                }
                modelList
            }

            if let error = store.lastError {
                ErrorBanner(message: error) { store.lastError = nil }
                    .padding(.horizontal, 10)
                    .padding(.top, 6)
            }

            MenuDivider()

            HStack(spacing: 6) {
                FooterButton(title: "Models", systemImage: "arrow.down.circle",
                             badge: store.downloads.isEmpty ? nil : "\(store.downloads.count)") {
                    open("library")
                }
                .help(store.downloads.isEmpty ? "Download models" : "\(store.downloads.count) downloads in progress")
                FooterButton(title: "Settings", systemImage: "gearshape") {
                    open("settings")
                }
                FooterButton(title: "Quit", systemImage: "power") {
                    confirmQuit()
                }
                .help("Quit Local Router")
            }
            .padding(.horizontal, 10)
            .padding(.bottom, 10)
        }
        .frame(width: 370)
        // The content's own height, which the window is then sized to.
        .fixedSize(horizontal: false, vertical: true)
        .background(MenuWindowSizer(radius: 18))
    }

    private func open(_ window: String) {
        dismiss()
        openWindow(id: window)
        NSApp.activate()
    }

    /// Quitting Local Router doesn't stop Ollama, so say what keeps running.
    /// Quits right away when nothing is loaded or downloading.
    private func confirmQuit() {
        let busy = store.loaded.filter { (store.activity[$0.name] ?? .idle) != .idle }
        let names = { (models: [RunningModel]) in
            ListFormatter.localizedString(byJoining: models.map { store.aliases[$0.name] ?? ModelIdentity.prettyName($0.name) })
        }
        guard !store.loaded.isEmpty || !store.downloads.isEmpty else {
            NSApp.terminate(nil)
            return
        }

        let alert = NSAlert()
        alert.icon = .localRouterMark(size: 64)
        if !busy.isEmpty || !store.downloads.isEmpty {
            alert.alertStyle = .warning
            alert.messageText = busy.isEmpty ? "Downloads Are Still Running" : "\(names(busy)) Is Still Working"
            var info: [String] = []
            if !busy.isEmpty {
                info.append("It keeps running in Ollama after Local Router quits; you just won't see it here.")
            }
            if !store.downloads.isEmpty {
                info.append("\(store.downloads.count == 1 ? "A download" : "\(store.downloads.count) downloads") will stop.")
            }
            alert.informativeText = info.joined(separator: " ")
        } else {
            alert.messageText = "Quit Local Router?"
            alert.informativeText = "Ollama keeps \(names(store.loaded)) loaded after Local Router quits."
        }
        alert.addButton(withTitle: "Quit")
        alert.addButton(withTitle: "Eject All and Quit")
        alert.addButton(withTitle: "Cancel")
        if store.loaded.isEmpty { alert.buttons[1].isHidden = true }

        dismiss()
        NSApp.activate()
        switch alert.runModal() {
        case .alertFirstButtonReturn:
            NSApp.terminate(nil)
        case .alertSecondButtonReturn:
            Task {
                await store.ejectAll()
                NSApp.terminate(nil)
            }
        default:
            break
        }
    }

    private var header: some View {
        HStack(alignment: .center, spacing: 7) {
            AppBadge(size: 20)
            Text("Local Router")
                .font(.system(size: 13, weight: .semibold))
            AskantisLink()
            Spacer()
            StatusLabel(status: store.status)
        }
    }

    private var modelList: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                // Hidden until the first model is favorited.
                if !store.favoriteModels.isEmpty {
                    SectionHeader("Favorites", count: store.favoriteModels.count, isCollapsed: $favoritesCollapsed) { EmptyView() }
                        .padding(.top, 6)
                    if !favoritesCollapsed {
                        ForEach(store.favoriteModels) { model in
                            InstalledModelRow(model: model, isLoading: store.pending[model.name] == .loading)
                                .transition(.opacity.combined(with: .move(edge: .bottom)))
                        }
                    }
                }

                SectionHeader("On This Mac", count: store.otherModels.count, isCollapsed: $installedCollapsed) { EmptyView() }
                    .padding(.top, 6)
                if !installedCollapsed {
                    if let storage = store.storage {
                        StorageCard(storage: storage, models: store.modelsDiskUsage)
                            .padding(.horizontal, 5)
                            .padding(.bottom, 4)
                    }
                    if store.otherModels.isEmpty {
                        EmptyRow(text: store.installed.isEmpty ? "No models installed yet"
                                 : store.favoriteModels.isEmpty ? "All models are loaded" : "Everything else is in Favorites or loaded")
                    }
                    ForEach(store.otherModels) { model in
                        InstalledModelRow(model: model, isLoading: store.pending[model.name] == .loading)
                    }
                }
            }
            .padding(.horizontal, 5)
            .padding(.vertical, 4)
        }
        .frame(maxHeight: 460)
        .fixedSize(horizontal: false, vertical: true)
    }
}

// MARK: - Building blocks

struct MenuDivider: View {
    var body: some View {
        Divider().padding(.horizontal, 14).padding(.vertical, 5)
    }
}

/// The app icon, small, like the model icons in the list.
struct AppBadge: View {
    let size: CGFloat

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: size * 0.28, style: .continuous)
        ZStack {
            shape.fill(LinearGradient(colors: [Color(red: 0.35, green: 0.45, blue: 1), Color(red: 0.16, green: 0.2, blue: 0.62)],
                                      startPoint: .top, endPoint: .bottom))
            Image(nsImage: .localRouterMark(size: size * 0.72))
                .renderingMode(.template)
                .foregroundStyle(.white)
        }
        .frame(width: size, height: size)
    }
}

/// "by Askantis", brightening and underlining on hover.
private struct AskantisLink: View {
    @State private var hovering = false

    var body: some View {
        Text("by Askantis")
            .font(.system(size: 10))
            .foregroundStyle(hovering ? AnyShapeStyle(.primary) : AnyShapeStyle(.secondary))
            .underline(hovering)
            .contentShape(Rectangle())
            .onHover { hovering = $0 }
            .onTapGesture { NSWorkspace.shared.open(URL(string: "https://askantis.ch")!) }
            .pointerStyle(.link)
            .help("askantis.ch")
            .animation(.easeOut(duration: 0.12), value: hovering)
    }
}

/// A square icon button with its name underneath, like Control Center's.
struct FooterButton: View {
    let title: String
    let systemImage: String
    var badge: String? = nil
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            VStack(spacing: 4) {
                Image(systemName: systemImage)
                    .font(.system(size: 15, weight: .medium))
                    .frame(height: 18)
                    .overlay(alignment: .topTrailing) {
                        if let badge {
                            Text(badge)
                                .font(.system(size: 8, weight: .bold))
                                .foregroundStyle(.white)
                                .padding(.horizontal, 4)
                                .padding(.vertical, 1)
                                .background(Color.accentColor, in: Capsule())
                                .offset(x: 9, y: -5)
                        }
                    }
                Text(title)
                    .font(.system(size: 11, weight: .medium))
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 8)
            .background(.quaternary.opacity(hovering ? 0.9 : 0.45), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
            .contentShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
        }
        .buttonStyle(.plain)
        .pointerStyle(.link)
        .onHover { hovering = $0 }
    }
}

/// A section title that collapses its rows when clicked, like Finder's sidebar.
/// The collapsed state is stored, so it survives restarts.
private struct SectionHeader<Accessory: View>: View {
    let title: String
    let count: Int
    @Binding var isCollapsed: Bool
    @ViewBuilder let accessory: () -> Accessory
    @State private var hovering = false

    init(_ title: String, count: Int, isCollapsed: Binding<Bool>, @ViewBuilder accessory: @escaping () -> Accessory) {
        self.title = title
        self.count = count
        self._isCollapsed = isCollapsed
        self.accessory = accessory
    }

    var body: some View {
        HStack(spacing: 5) {
            Button {
                withAnimation(.snappy(duration: 0.2)) { isCollapsed.toggle() }
            } label: {
                HStack(spacing: 5) {
                    Text(title)
                        .font(.system(size: 13, weight: .semibold))
                    if isCollapsed && count > 0 {
                        Text("\(count)")
                            .font(.system(size: 11, weight: .medium))
                            .monospacedDigit()
                    }
                    Image(systemName: "chevron.right")
                        .font(.system(size: 9, weight: .bold))
                        .rotationEffect(.degrees(isCollapsed ? 0 : 90))
                        .opacity(hovering || isCollapsed ? 1 : 0)
                }
                .foregroundStyle(.secondary)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(isCollapsed ? "Show \(title)" : "Hide \(title)")
            Spacer()
            accessory()
        }
        .onHover { hovering = $0 }
        .padding(.horizontal, 9)
        .padding(.top, 8)
        .padding(.bottom, 3)
    }
}

private struct EmptyRow: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.system(size: 13))
            .foregroundStyle(.tertiary)
            .padding(.horizontal, 9)
            .padding(.vertical, 6)
    }
}

private struct StatusLabel: View {
    let status: ModelStore.Status

    var body: some View {
        HStack(spacing: 5) {
            Circle()
                .fill(color)
                .frame(width: 6, height: 6)
            Text(text)
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
        }
        .help(help)
    }

    private var color: Color {
        switch status {
        case .checking: .gray
        case .running: .green
        case .offline: .red
        }
    }

    private var help: String {
        if case .running(let version) = status { return "Ollama \(version) is running" }
        return text
    }

    private var text: String {
        switch status {
        case .checking: "Connecting…"
        case .running: "Ollama"
        case .offline: "Ollama not running"
        }
    }
}

private struct OfflineView: View {
    @Environment(ModelStore.self) private var store

    var body: some View {
        VStack(spacing: 6) {
            Image(systemName: "bolt.horizontal.circle")
                .font(.system(size: 34, weight: .light))
                .foregroundStyle(.secondary)
                .padding(.bottom, 4)
            Text("Ollama Isn't Running")
                .font(.system(size: 13, weight: .semibold))
            Text("Start Ollama to see and load your models.")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
            HStack(spacing: 8) {
                Button("Try Again") { Task { await store.refresh() } }
                Button("Start Ollama") { store.startOllama() }
                    .keyboardShortcut(.defaultAction)
            }
            .controlSize(.regular)
            .padding(.top, 8)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 20)
    }
}

private struct ErrorBanner: View {
    let message: String
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.yellow)
            Text(message)
                .font(.system(size: 11))
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
            Button(action: dismiss) {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(.tertiary)
            }
            .buttonStyle(.plain)
        }
        .padding(10)
        .background(.quaternary.opacity(0.6), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}
