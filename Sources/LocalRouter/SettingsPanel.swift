import ServiceManagement
import SwiftUI

/// Settings pages, in the order the sidebar shows them.
enum SettingsPane: String, CaseIterable, Identifiable {
    case general, menuBar, models, schedule, storage, ollama, sharing, advanced, credits

    var id: String { rawValue }

    var title: String {
        switch self {
        case .general: "General"
        case .menuBar: "Menu Bar"
        case .models: "Models"
        case .schedule: "Schedule"
        case .storage: "Models Folder"
        case .ollama: "Ollama"
        case .sharing: "Sharing"
        case .advanced: "Advanced"
        case .credits: "Credits"
        }
    }

    var systemImage: String {
        switch self {
        case .general: "gearshape.fill"
        case .menuBar: "menubar.rectangle"
        case .models: "cpu.fill"
        case .schedule: "calendar.badge.clock"
        case .storage: "externaldrive.fill"
        case .ollama: "server.rack"
        case .sharing: "network.badge.shield.half.filled"
        case .advanced: "slider.horizontal.3"
        case .credits: "heart.fill"
        }
    }

    /// Words that find this page in the sidebar search.
    var keywords: [String] {
        switch self {
        case .general: ["login", "startup", "start", "welcome", "log", "stream", "launch"]
        case .menuBar: ["icon", "memory", "context", "uptime", "percent", "status bar"]
        case .models: ["default", "keep", "loaded", "idle", "mlx", "gguf", "favorite"]
        case .schedule: ["time", "days", "hours", "weekday", "keep loaded"]
        case .storage: ["folder", "disk", "drive", "external", "move", "symlink", "location"]
        case .ollama: ["address", "host", "port", "connection", "find", "status"]
        case .sharing: ["network", "share", "key", "token", "api", "auth", "password", "expose", "lan", "remote"]
        case .advanced: ["context", "length", "memory saver", "kv", "cache", "flash", "attention", "parallel", "loaded at once", "unload"]
        case .credits: ["about", "version", "license", "askantis", "author", "thanks", "privacy"]
        }
    }

    var color: Color {
        switch self {
        case .general: .gray
        case .menuBar: .blue
        case .models: .indigo
        case .schedule: .red
        case .storage: .orange
        case .ollama: .green
        case .sharing: .teal
        case .advanced: .purple
        case .credits: .pink
        }
    }
}

/// The Settings window: a sidebar of pages, like System Settings. It reopens
/// on the page you had open last.
struct SettingsView: View {
    @AppStorage("settingsPane") private var pane = SettingsPane.general
    @State private var search = ""

    private var panes: [SettingsPane] {
        let query = search.trimmingCharacters(in: .whitespaces).lowercased()
        guard !query.isEmpty else { return SettingsPane.allCases }
        return SettingsPane.allCases.filter { pane in
            pane.title.lowercased().contains(query) || pane.keywords.contains { $0.contains(query) || query.contains($0) }
        }
    }

    var body: some View {
        NavigationSplitView {
            List(selection: Binding(get: { pane }, set: { if let new = $0 { pane = new } })) {
                // Room between the search field and the first page.
                Color.clear
                    .frame(height: 6)
                    .listRowInsets(EdgeInsets())
                    .selectionDisabled()
                ForEach(panes) { item in
                    Label {
                        Text(item.title)
                    } icon: {
                        SettingsIcon(systemImage: item.systemImage, color: item.color)
                    }
                    .tag(item)
                }
            }
            .listStyle(.sidebar)
            .searchable(text: $search, placement: .sidebar, prompt: "Search")
            .onChange(of: search) { _, _ in
                // Jump to the first match, like System Settings.
                if let first = panes.first, !panes.contains(pane) { pane = first }
            }
            .navigationSplitViewColumnWidth(min: 190, ideal: 200, max: 240)
            // The sidebar is the menu of pages; it shouldn't be hidden.
            .toolbar(removing: .sidebarToggle)
        } detail: {
            Group {
                switch pane {
                case .general: GeneralPane()
                case .menuBar: MenuBarPane()
                case .models: ModelsPane()
                case .schedule: SchedulePane()
                case .storage: StoragePane()
                case .ollama: OllamaPane()
                case .sharing: SharingPane()
                case .advanced: AdvancedSettingsView()
                case .credits: CreditsPane()
                }
            }
            .formStyle(.grouped)
            .id(pane)
        }
        .windowHeader("Settings", detail: pane.title)
        .frame(minWidth: 760, idealWidth: 780, minHeight: 540, idealHeight: 620)
        .background(BringToFront())
        .onOpenURL { url in
            if url.host == "advanced" { pane = .advanced }
        }
    }
}

/// Who made Local Router, and what it builds on.
private struct CreditsPane: View {
    private var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "1.0"
        let build = info?["CFBundleVersion"] as? String ?? ""
        return build.isEmpty ? "Version \(short)" : "Version \(short) (\(build))"
    }

    var body: some View {
        Form {
            Section {
                VStack(spacing: 8) {
                    AppBadge(size: 64)
                    Text("Local Router")
                        .font(.system(size: 18, weight: .bold))
                    Text(version)
                        .font(.system(size: 12))
                        .foregroundStyle(.secondary)
                    Text("Your local AI models, one click away in the menu bar.")
                        .font(.system(size: 12))
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 8)
            }

            Section("Made By") {
                LabeledContent("Author") {
                    Link("Mikel Prandini", destination: URL(string: "https://www.linkedin.com/in/mikel-prandini")!)
                }
                LabeledContent("GitHub") {
                    Link("MikiTheAlien", destination: URL(string: "https://github.com/MikiTheAlien")!)
                }
                LabeledContent("Published by") {
                    Link("Askantis", destination: URL(string: "https://askantis.ch")!)
                }
                LabeledContent("Source Code") {
                    Link("github.com/Askantis/public-local-router", destination: URL(string: "https://github.com/Askantis/public-local-router")!)
                }
            }

            Section("Built On") {
                LabeledContent("Ollama") { Link("ollama.com", destination: URL(string: "https://ollama.com")!) }
                LabeledContent("llama.cpp") { Link("github.com/ggml-org/llama.cpp", destination: URL(string: "https://github.com/ggml-org/llama.cpp")!) }
                LabeledContent("Model Logos") { Text("Hugging Face organisations; they belong to their owners").foregroundStyle(.secondary) }
                LabeledContent("Model Descriptions") { Link("Ollama library", destination: URL(string: "https://ollama.com/library")!) }
            }

            Section {
                Label("No accounts, no analytics, no tracking. Local Router collects nothing and sends nothing about you anywhere.", systemImage: "hand.raised.fill")
                    .fixedSize(horizontal: false, vertical: true)
            } header: {
                Text("Privacy")
            } footer: {
                Text("Free for non-commercial use. Share it with credit. See the license for details.")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}

/// A white symbol on a coloured rounded square, like System Settings.
private struct SettingsIcon: View {
    let systemImage: String
    let color: Color

    var body: some View {
        Image(systemName: systemImage)
            .font(.system(size: 11, weight: .semibold))
            .foregroundStyle(.white)
            .frame(width: 22, height: 22)
            .background(color.gradient, in: RoundedRectangle(cornerRadius: 6, style: .continuous))
    }
}

// MARK: - Pages

private struct GeneralPane: View {
    @Environment(ModelStore.self) private var store
    @Environment(\.openWindow) private var openWindow
    @AppStorage(SettingsKey.startOllamaAtStartup) private var startOllama = false
    @AppStorage(SettingsKey.loadDefaultAtStartup) private var loadDefault = false
    @State private var openAtLogin = SMAppService.mainApp.status == .enabled

    var body: some View {
        Form {
            Section {
                Toggle("Open at Login", isOn: $openAtLogin)
                    .onChange(of: openAtLogin) { _, enabled in
                        do {
                            if enabled {
                                try SMAppService.mainApp.register()
                            } else {
                                try SMAppService.mainApp.unregister()
                            }
                        } catch {
                            store.lastError = "Open at Login: \(error.localizedDescription)"
                            openAtLogin = SMAppService.mainApp.status == .enabled
                        }
                    }
                Toggle("Start Ollama If It's Not Running", isOn: $startOllama)
                Toggle("Load Default Model at Startup", isOn: $loadDefault)
                    .disabled(store.defaultModel == nil)
                    .help(store.defaultModel == nil ? "Choose a default model first" : "Also starts Ollama if needed")
            } header: {
                Text("Startup")
            }
            Section {
                LabeledContent("Welcome Guide") {
                    Button("Show…") {
                        openWindow(id: "onboarding")
                        NSApp.activate()
                    }
                }
                LabeledContent("Log Stream") {
                    Button("Open…") {
                        openWindow(id: "logs")
                        NSApp.activate()
                    }
                }
            }
        }
        .onAppear { openAtLogin = SMAppService.mainApp.status == .enabled }
    }
}

private struct MenuBarPane: View {
    @AppStorage(SettingsKey.menuBarMemory) private var showMemory = false
    @AppStorage(SettingsKey.menuBarContext) private var showContext = true
    @AppStorage(SettingsKey.menuBarUptime) private var showUptime = false

    var body: some View {
        Form {
            Section {
                Toggle("Model Memory", isOn: $showMemory)
                Toggle("Context Used", isOn: $showContext)
                Toggle("Uptime", isOn: $showUptime)
            } header: {
                Text("Next to the Icon")
            } footer: {
                Text(preview)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    /// What the menu bar will show, e.g. "13 GB | 24% | 1h".
    private var preview: String {
        var parts: [String] = []
        if showMemory { parts.append("13 GB") }
        if showContext { parts.append("24%") }
        if showUptime { parts.append("1h") }
        return parts.isEmpty ? "Just the icon." : "Shows: " + parts.joined(separator: " | ")
    }
}

private struct ModelsPane: View {
    @Environment(ModelStore.self) private var store
    @AppStorage(SettingsKey.keepAlive) private var keepAlive = KeepAlive.fiveMinutes.rawValue
    @AppStorage(SettingsKey.preferMLX) private var preferMLX = SettingsKey.preferMLXDefault

    var body: some View {
        Form {
            Section {
                Picker("Default Model", selection: Binding(
                    get: { store.defaultModel ?? "" },
                    set: { store.defaultModel = $0.isEmpty ? nil : $0 }
                )) {
                    Text("None").tag("")
                    ForEach(store.installed) { model in
                        Text(store.aliases[model.name] ?? ModelIdentity.prettyName(model.name)).tag(model.name)
                    }
                }
                Picker("Keep Loaded For", selection: $keepAlive) {
                    ForEach(KeepAlive.allCases) { option in
                        Text(option.label).tag(option.rawValue)
                    }
                }
                Toggle("Prefer MLX Builds", isOn: $preferMLX)
            } footer: {
                Text("The default model is starred in the menu. Your first download becomes the default. MLX builds run best on Apple silicon.")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}

private struct SchedulePane: View {
    var body: some View {
        Form { ScheduleSection() }
    }
}

private struct StoragePane: View {
    var body: some View {
        Form {
            Section { ModelsFolderView() }
        }
    }
}

private struct OllamaPane: View {
    @Environment(ModelStore.self) private var store
    @AppStorage(SettingsKey.ollamaURL) private var ollamaURL = "http://localhost:11434"
    @AppStorage("settingsPane") private var pane = SettingsPane.general
    @State private var searching = false
    @State private var findResult: String?

    var body: some View {
        Form {
            Section {
                LabeledContent("Status") {
                    ConnectionBadge(status: store.status)
                }
                LabeledContent("Address") {
                    HStack(spacing: 8) {
                        TextField("Address", text: $ollamaURL)
                            .labelsHidden()
                            .multilineTextAlignment(.trailing)
                            .onSubmit { Task { await store.refresh() } }
                        Button {
                            Task { await findOllama() }
                        } label: {
                            if searching {
                                ProgressView().controlSize(.small)
                            } else {
                                Label("Find", systemImage: "magnifyingglass")
                            }
                        }
                        .disabled(searching)
                        .help("Look for a running Ollama on this Mac")
                    }
                }
                LabeledContent("Server Settings") {
                    Button("Advanced…") { pane = .advanced }
                }
            } footer: {
                Text(findResult ?? "Default: \(OllamaFinder.defaultAddress)")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .onAppear {
            // Don't start with the address field selected.
            DispatchQueue.main.async { NSApp.keyWindow?.makeFirstResponder(nil) }
        }
    }

    private func findOllama() async {
        searching = true
        defer { searching = false }
        if let found = await OllamaFinder.find() {
            ollamaURL = found.address
            findResult = "Found Ollama \(found.version) at \(found.address)."
            await store.refresh()
        } else {
            findResult = "No running Ollama found. Start it, then try again."
        }
    }
}

/// A green dot and "Connected" when Ollama answers, red when it doesn't.
private struct ConnectionBadge: View {
    let status: ModelStore.Status

    var body: some View {
        HStack(spacing: 6) {
            switch status {
            case .running(let version):
                Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
                Text("Connected · Ollama \(version)")
            case .checking:
                ProgressView().controlSize(.small)
                Text("Connecting…")
            case .offline:
                Image(systemName: "xmark.circle.fill").foregroundStyle(.red)
                Text("Not connected")
            }
        }
    }
}

/// "Keep a model loaded" on chosen days and hours.
private struct ScheduleSection: View {
    @Environment(ModelStore.self) private var store

    var body: some View {
        @Bindable var store = store
        Section {
            Toggle("Keep a Model Loaded on a Schedule", isOn: $store.schedule.enabled)
            if store.schedule.enabled {
                Picker("Model", selection: $store.schedule.model) {
                    Text(store.defaultModel.map { "Default model (\(name($0)))" } ?? "Default model (none set)").tag(String?.none)
                    ForEach(store.installed) { model in
                        Text(name(model.name)).tag(String?.some(model.name))
                    }
                }
                LabeledContent("Days") {
                    HStack(spacing: 4) {
                        ForEach(Self.weekdays, id: \.number) { day in
                            DayToggle(label: day.letter, isOn: Binding(
                                get: { store.schedule.days.contains(day.number) },
                                set: { on in
                                    if on { store.schedule.days.insert(day.number) } else { store.schedule.days.remove(day.number) }
                                }
                            ))
                            .help(day.name)
                        }
                    }
                }
                DatePicker("From", selection: time(\.start), displayedComponents: .hourAndMinute)
                DatePicker("Until", selection: time(\.end), displayedComponents: .hourAndMinute)
            }
        } header: {
            Text("Schedule")
        } footer: {
            Text(store.schedule.enabled
                 ? "Local Router starts Ollama if needed, loads the model and keeps it loaded. Afterwards it unloads when idle, like any other model."
                 : "For example, have your writing model ready every weekday from 9 to 6.")
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func name(_ model: String) -> String {
        store.aliases[model] ?? ModelIdentity.prettyName(model)
    }

    /// Monday first, as in Swiss and European calendars.
    private static let weekdays: [(number: Int, letter: String, name: String)] = [
        (2, "M", "Monday"), (3, "T", "Tuesday"), (4, "W", "Wednesday"), (5, "T", "Thursday"),
        (6, "F", "Friday"), (7, "S", "Saturday"), (1, "S", "Sunday"),
    ]

    /// Minutes after midnight as a Date for DatePicker.
    private func time(_ keyPath: WritableKeyPath<KeepLoadedSchedule, Int>) -> Binding<Date> {
        Binding(
            get: {
                Calendar.current.startOfDay(for: .now).addingTimeInterval(TimeInterval(store.schedule[keyPath: keyPath] * 60))
            },
            set: { date in
                let parts = Calendar.current.dateComponents([.hour, .minute], from: date)
                store.schedule[keyPath: keyPath] = (parts.hour ?? 0) * 60 + (parts.minute ?? 0)
            }
        )
    }
}

private struct DayToggle: View {
    let label: String
    @Binding var isOn: Bool

    var body: some View {
        Button { isOn.toggle() } label: {
            Text(label)
                .font(.system(size: 11, weight: .semibold))
                .frame(width: 22, height: 22)
                .foregroundStyle(isOn ? AnyShapeStyle(.white) : AnyShapeStyle(.secondary))
                .background(isOn ? AnyShapeStyle(Color.accentColor) : AnyShapeStyle(.quaternary), in: Circle())
        }
        .buttonStyle(.plain)
        .pointerStyle(.link)
    }
}
