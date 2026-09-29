import AppKit
import Foundation
import Network
import SwiftUI

/// How this Mac's models can be reached from the network.
enum SharingMode: String, CaseIterable, Identifiable {
    /// Only apps on this Mac.
    case off
    /// Local Router forwards requests that carry an access key.
    case keys
    /// Ollama listens on the network itself, with no password.
    case direct

    var id: String { rawValue }

    var label: String {
        switch self {
        case .off: "Only This Mac"
        case .keys: "Share with Access Keys"
        case .direct: "Expose Ollama Directly"
        }
    }
}

extension ModelStore {
    static let sharePortDefault = 11435

    var sharingMode: SharingMode {
        get { SharingMode(rawValue: sharingModeValue) ?? .off }
        set {
            let old = sharingMode
            sharingModeValue = newValue.rawValue
            UserDefaults.standard.set(newValue.rawValue, forKey: SettingsKey.sharingMode)
            // Direct exposure is an Ollama setting; turn it on only for that mode.
            var settings = serverSettings
            settings.exposeToNetwork = newValue == .direct
            serverSettings = settings
            restartSharing()
            // Ollama only picks up OLLAMA_HOST when it restarts, so entering or
            // leaving Direct restarts it: otherwise it would stay exposed.
            if (old == .direct) != (newValue == .direct) {
                Task { await applyServerSettings() }
            }
        }
    }

    var accessKeys: [AccessKey] {
        get { accessKeysValue }
        set {
            accessKeysValue = newValue
            UserDefaults.standard.set(try? JSONEncoder().encode(newValue), forKey: SettingsKey.accessKeys)
            shareProxy?.setKeys(newValue)
        }
    }

    /// Creates a key and returns its secret, which is shown once.
    func createAccessKey(named name: String) -> String {
        let (key, secret) = AccessKey.generate(name: name.isEmpty ? "Key \(accessKeys.count + 1)" : name)
        accessKeys.append(key)
        logStream.note("Created access key “\(key.name)”")
        return secret
    }

    func revokeAccessKey(_ key: AccessKey) {
        accessKeys.removeAll { $0.id == key.id }
        logStream.note("Revoked access key “\(key.name)”", tone: .warning)
    }

    static func loadAccessKeys() -> [AccessKey] {
        guard let data = UserDefaults.standard.data(forKey: SettingsKey.accessKeys),
              let keys = try? JSONDecoder().decode([AccessKey].self, from: data) else { return [] }
        return keys
    }

    /// Starts, stops or restarts the key-checking proxy to match the settings.
    func restartSharing() {
        shareProxy?.stop()
        shareProxy = nil
        shareState = .stopped
        guard sharingMode == .keys else { return }

        let upstream = URLComponents(string: UserDefaults.standard.string(forKey: SettingsKey.ollamaURL) ?? "")
        let proxy = ShareProxy(upstreamHost: "127.0.0.1", upstreamPort: UInt16(exactly: upstream?.port ?? 11434) ?? 11434) { event in
            Task { @MainActor in ModelStore.current?.noteShareEvent(event) }
        }
        proxy.setKeys(accessKeys)
        do {
            let identity = shareHTTPS ? try ShareCertificate.identity() : nil
            try proxy.start(port: UInt16(exactly: sharePort) ?? UInt16(Self.sharePortDefault), identity: identity) { state in
                Task { @MainActor in ModelStore.current?.updateShareState(state) }
            }
            shareProxy = proxy
            shareState = .starting
        } catch {
            shareState = .failed(error.localizedDescription)
        }
    }

    func updateShareState(_ state: NWListener.State) {
        switch state {
        case .ready:
            shareState = .running
            logStream.note("Sharing on port \(sharePort)\(shareHTTPS ? " with HTTPS" : ""); requests need an access key", tone: .good)
        case .failed(let error):
            shareState = .failed(error.localizedDescription)
            logStream.note("Sharing stopped: \(error.localizedDescription)", tone: .error)
        case .cancelled:
            shareState = .stopped
        default:
            break
        }
    }

    func noteShareEvent(_ event: ShareProxy.Event) {
        if event.accepted, let id = event.keyID, let index = accessKeys.firstIndex(where: { $0.id == id }) {
            var keys = accessKeys
            keys[index].lastUsed = .now
            accessKeysValue = keys
            UserDefaults.standard.set(try? JSONEncoder().encode(keys), forKey: SettingsKey.accessKeys)
        }
        logStream.appendNetwork(event)
    }

    /// Where other devices connect, e.g. https://Mikis-Mac-Studio.local:11435
    var shareURLs: [String] {
        let scheme = shareHTTPS ? "https" : "http"
        return ([LocalAddresses.hostName] + LocalAddresses.ipv4()).map { "\(scheme)://\($0):\(sharePort)" }
    }
}

enum ShareState: Equatable {
    case stopped, starting, running
    case failed(String)
}

extension LogStream {
    func appendNetwork(_ event: ShareProxy.Event) {
        let who = event.keyName.map { "\(event.client) · \($0)" } ?? event.client
        if event.accepted {
            appendEvent(LogLine(date: .now, kind: .request, source: event.client, text: "\(event.method) \(event.path)  via Sharing (\(who))"))
        } else {
            appendEvent(LogLine(date: .now, kind: .request, source: event.client,
                                text: "Rejected \(event.method) \(event.path): no valid access key", tone: .error))
        }
    }
}

// MARK: - Settings page

struct SharingPane: View {
    @Environment(ModelStore.self) private var store
    @State private var createdSecret: String?
    @State private var naming = false
    @State private var newKeyName = ""
    @State private var confirmDirect = false
    @State private var showOptions = false
    @State private var showCertificateHelp = false

    var body: some View {
        Form {
            Section {
                Picker("Who Can Use Your Models", selection: Binding(
                    get: { store.sharingMode },
                    set: { mode in
                        if mode == .direct { confirmDirect = true } else { store.sharingMode = mode }
                    }
                )) {
                    Text("Only this Mac").tag(SharingMode.off)
                    Text("Devices with an access key (recommended)").tag(SharingMode.keys)
                    Text("Anyone on the network, no password").tag(SharingMode.direct)
                }
                .pickerStyle(.radioGroup)
            }

            switch store.sharingMode {
            case .off: EmptyView()
            case .keys: keysSections
            case .direct: directSection
            }
        }
        .alert("Share Without a Password?", isPresented: $confirmDirect) {
            Button("Share", role: .destructive) { store.sharingMode = .direct }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Anyone on your network could use, download and delete your models. Access keys are safer.")
        }
        .alert("Name the New Key", isPresented: $naming) {
            TextField("e.g. MacBook Air", text: $newKeyName)
            Button("Create") {
                createdSecret = store.createAccessKey(named: newKeyName)
                newKeyName = ""
            }
            Button("Cancel", role: .cancel) { newKeyName = "" }
        } message: {
            Text("One key per device or app, so you can revoke them separately.")
        }
        .sheet(item: Binding(get: { createdSecret.map(Secret.init) }, set: { if $0 == nil { createdSecret = nil } })) { secret in
            NewKeySheet(secret: secret.value) { createdSecret = nil }
        }
    }

    private struct Secret: Identifiable { let value: String; var id: String { value } }

    @ViewBuilder private var keysSections: some View {
        Section {
            LabeledContent {
                statusBadge
            } label: {
                Text("Status")
            }
            if case .running = store.shareState, let address = store.shareURLs.first {
                LabeledContent("Address") { CopyableText(text: address) }
            }
        }

        Section {
            ForEach(store.accessKeys) { key in
                HStack {
                    Image(systemName: "key.fill").foregroundStyle(.secondary)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(key.name)
                        Text(key.lastUsed.map { "Used \($0.formatted(.relative(presentation: .named)))" } ?? "Not used yet")
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Revoke", role: .destructive) { store.revokeAccessKey(key) }
                }
            }
            Button {
                naming = true
            } label: {
                Label("Create Key…", systemImage: "plus")
            }
        } header: {
            Text("Access Keys")
        } footer: {
            note(store.accessKeys.isEmpty ? "Create a key for each device that should use your models." : "A key is shown once, when you create it.")
        }

        Section {
            LabeledContent("Base URL") { CopyableText(text: (store.shareURLs.first ?? "") + "/v1") }
            LabeledContent("API Key") { Text("Your access key").foregroundStyle(.secondary) }
            if store.shareHTTPS {
                LabeledContent {
                    HStack(spacing: 8) {
                        Button("Export…", action: exportCertificate)
                        Button { showCertificateHelp.toggle() } label: { Image(systemName: "info.circle") }
                            .buttonStyle(.borderless)
                            .popover(isPresented: $showCertificateHelp, arrowEdge: .bottom) { certificateHelp }
                    }
                } label: {
                    Text("Certificate")
                }
            }
        } header: {
            Text("Connect an App")
        } footer: {
            note("Add it like any OpenAI-compatible provider, for example in Hermes.")
        }

        Section {
            DisclosureGroup("Options", isExpanded: $showOptions) {
                Toggle("Encrypt with HTTPS", isOn: Binding(get: { store.shareHTTPS }, set: { store.shareHTTPS = $0 }))
                LabeledContent("Port") {
                    TextField("Port", value: Binding(get: { store.sharePort }, set: { store.sharePort = $0 }), format: .number.grouping(.never))
                        .labelsHidden()
                        .multilineTextAlignment(.trailing)
                        .frame(width: 70)
                }
                ForEach(store.shareURLs.dropFirst(), id: \.self) { url in
                    LabeledContent("Also Reachable At") { CopyableText(text: url) }
                }
                if store.shareHTTPS {
                    LabeledContent("Certificate") {
                        Button("Make a New One") {
                            try? ShareCertificate.regenerate()
                            store.restartSharing()
                        }
                    }
                    if let fingerprint = ShareCertificate.fingerprint {
                        LabeledContent("Fingerprint") {
                            Text(fingerprint)
                                .font(.system(size: 9, design: .monospaced))
                                .textSelection(.enabled)
                                .multilineTextAlignment(.trailing)
                        }
                    }
                }
            }
        }
    }

    @ViewBuilder private var statusBadge: some View {
        switch store.shareState {
        case .running:
            Label("On, for devices with a key", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
        case .starting:
            Text("Starting…").foregroundStyle(.secondary)
        case .stopped:
            Text("Off").foregroundStyle(.secondary)
        case .failed(let reason):
            HStack(spacing: 8) {
                Label(reason, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                Button("Try Again") { store.restartSharing() }
            }
        }
    }

    private var certificateHelp: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Why a Certificate?").font(.system(size: 13, weight: .semibold))
            Text("It encrypts the connection. Because it's made on this Mac, other devices don't know it yet, so give it to them once:")
            Text("• **Another Mac:** double-click the file, then set it to Always Trust in Keychain Access.")
            Text("• **Hermes and other Python apps:** set `SSL_CERT_FILE` to the file's path.")
            Text("Or turn off HTTPS under Options to skip this, on a network you trust.")
                .foregroundStyle(.secondary)
        }
        .font(.system(size: 12))
        .fixedSize(horizontal: false, vertical: true)
        .frame(width: 320)
        .padding(16)
    }

    private func exportCertificate() {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "Local Router Certificate.pem"
        panel.message = "Save the certificate to copy to your other devices."
        NSApp.activate()
        guard panel.runModal() == .OK, let url = panel.url else { return }
        try? FileManager.default.removeItem(at: url)
        try? FileManager.default.copyItem(at: ShareCertificate.certificateURL, to: url)
    }

    @ViewBuilder private var directSection: some View {
        Section {
            if !store.pendingSettingKeys.isEmpty {
                LabeledContent("Ollama") {
                    Button("Restart Ollama to Apply") { Task { await store.applyServerSettings() } }
                        .buttonStyle(.borderedProminent)
                }
            }
            ForEach(LocalAddresses.ipv4(), id: \.self) { address in
                LabeledContent("Address") { CopyableText(text: "http://\(address):11434") }
            }
        } footer: {
            note("No password and no encryption. Only on a network you trust.")
        }
    }

    private func note(_ text: String) -> some View {
        Text(text)
            .foregroundStyle(.secondary)
            .multilineTextAlignment(.leading)
            .frame(maxWidth: .infinity, alignment: .leading)
            .fixedSize(horizontal: false, vertical: true)
    }
}

/// Text with a copy button.
private struct CopyableText: View {
    let text: String
    @State private var copied = false

    var body: some View {
        HStack(spacing: 6) {
            Text(text)
                .font(.system(size: 12, design: .monospaced))
                .textSelection(.enabled)
                .lineLimit(1)
                .truncationMode(.middle)
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(text, forType: .string)
                copied = true
                DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { copied = false }
            } label: {
                Image(systemName: copied ? "checkmark" : "doc.on.doc")
            }
            .buttonStyle(.borderless)
            .help("Copy")
        }
    }
}

/// Shows a new key once, with a copy button.
private struct NewKeySheet: View {
    let secret: String
    let done: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Label("Your New Access Key", systemImage: "key.fill")
                .font(.system(size: 15, weight: .semibold))
            Text("Copy it now. It won't be shown again.")
                .foregroundStyle(.secondary)
            CopyableText(text: secret)
                .padding(10)
                .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 8))
            HStack {
                Spacer()
                Button("Done", action: done)
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 440)
    }
}
