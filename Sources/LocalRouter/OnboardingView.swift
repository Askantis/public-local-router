import ServiceManagement
import SwiftUI
import UserNotifications

/// First-launch setup: checks Ollama, turns on the background service apps
/// use (required), offers the two optional permissions (notifications, open
/// at login), then points to the menu bar. The service is registered only
/// when its step is reached, so the "background item added" notice from
/// macOS comes with an explanation.
struct OnboardingView: View {
    @Environment(ModelStore.self) private var store
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage(SettingsKey.onboardingDone) private var onboardingDone = false
    @State private var step = Step.welcome
    @State private var daemon = DaemonStore.shared

    enum Step: Int, CaseIterable { case welcome, about, ollama, storage, service, permissions, done }

    var body: some View {
        VStack(spacing: 0) {
            Group {
                switch step {
                case .welcome: WelcomeStep()
                case .about: AboutStep()
                case .ollama: OllamaStep()
                case .storage: StorageStep()
                case .service: ServiceStep()
                case .permissions: PermissionsStep()
                case .done: DoneStep()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .transition(.asymmetric(insertion: .move(edge: .trailing).combined(with: .opacity),
                                    removal: .move(edge: .leading).combined(with: .opacity)))
            .id(step)

            HStack {
                HStack(spacing: 6) {
                    ForEach(Step.allCases, id: \.self) { item in
                        Capsule()
                            .fill(item == step ? Color.accentColor : Color.secondary.opacity(0.3))
                            .frame(width: item == step ? 16 : 6, height: 6)
                    }
                }
                Spacer()
                if step != .welcome {
                    Button("Back") { go(-1) }
                        .controlSize(.large)
                }
                Button(primaryTitle) {
                    if step == .done { finish() } else { go(1) }
                }
                .keyboardShortcut(.defaultAction)
                .controlSize(.large)
                // The service is what apps use: no way around it.
                .disabled(step == .service && !daemon.running)
            }
            .padding(24)
        }
        .frame(width: 560, height: 480)
        .background(BringToFront())
        .background(.background)
        .task { NSApp.activate() }
    }

    private var primaryTitle: String {
        switch step {
        case .welcome: "Get Started"
        case .ollama: store.status == .offline ? "Skip for Now" : "Continue"
        case .about, .storage, .service, .permissions: "Continue"
        case .done: "Done"
        }
    }

    private func go(_ delta: Int) {
        withAnimation(.snappy(duration: 0.3)) {
            step = Step(rawValue: step.rawValue + delta) ?? step
        }
    }

    private func finish() {
        onboardingDone = true
        dismissWindow(id: "onboarding")
    }
}

// MARK: - Steps

private struct StepLayout<Content: View>: View {
    let systemImage: String
    var mark: NSImage? = nil
    /// A full-colour logo that fills the tile instead of an icon.
    var logo: NSImage? = nil
    let color: Color
    let title: String
    let subtitle: String
    @ViewBuilder let content: () -> Content

    var body: some View {
        VStack(spacing: 14) {
            ZStack {
                RoundedRectangle(cornerRadius: 20, style: .continuous)
                    .fill(color.gradient)
                if let logo {
                    Image(nsImage: logo)
                        .resizable()
                        .interpolation(.high)
                        .clipShape(RoundedRectangle(cornerRadius: 20, style: .continuous))
                }
                if logo != nil {
                    EmptyView()
                } else if let mark {
                    Image(nsImage: mark)
                        .renderingMode(.template)
                        .foregroundStyle(.white)
                } else {
                    Image(systemName: systemImage)
                        .font(.system(size: 34, weight: .medium))
                        .foregroundStyle(.white)
                }
            }
            .frame(width: 76, height: 76)
            .shadow(color: color.opacity(0.3), radius: 10, y: 4)
            .padding(.top, 36)

            Text(title)
                .font(.system(size: 22, weight: .bold))
            Text(subtitle)
                .font(.system(size: 13))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 400)
            content()
                .padding(.top, 8)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 40)
    }
}

private struct WelcomeStep: View {
    var body: some View {
        StepLayout(systemImage: "cpu.fill", mark: .localRouterMark(size: 44), color: .accentColor, title: "Welcome to Local Router",
                   subtitle: "Your local AI models, one click away in the menu bar.") {
            VStack(alignment: .leading, spacing: 14) {
                Feature(systemImage: "list.bullet.rectangle", title: "See What's Loaded",
                        text: "Every Ollama model on your Mac, and how much memory each one uses.")
                Feature(systemImage: "eject", title: "Load and Eject",
                        text: "Load a model before you need it, keep it alive, eject it when you're done.")
                Feature(systemImage: "arrow.down.circle", title: "Find Models That Fit",
                        text: "Recommendations based on your Mac's chip and memory.")
            }
            .frame(maxWidth: 400)
        }
    }
}

/// Who makes Local Router, and why.
private struct AboutStep: View {
    private static let logo = Bundle.main.url(forResource: "askantis", withExtension: "png").flatMap(NSImage.init(contentsOf:))

    var body: some View {
        StepLayout(systemImage: "lock.shield.fill", logo: Self.logo, color: .black, title: "Why We Built This",
                   subtitle: "Local Router is made by Askantis.") {
            VStack(alignment: .leading, spacing: 14) {
                Feature(systemImage: "lock.shield", title: "Local AI, Made Visible",
                        text: "Your data should stay yours, so we run local AI in our own workflows. We couldn't find a neat way to see what it was doing, so we built one.")
                Feature(systemImage: "building.2", title: "Who We Are",
                        text: "Askantis is a Swiss consulting firm that guides businesses through their AI transformation, with respect for data governance and AI independence.")
                HStack(spacing: 4) {
                    Text("Built by")
                    Link("Mikel Prandini", destination: URL(string: "https://www.linkedin.com/in/mikel-prandini")!)
                        .pointerStyle(.link)
                        .help("Mikel Prandini on LinkedIn")
                    Text("·")
                    Link("askantis.ch", destination: URL(string: "https://askantis.ch")!)
                        .pointerStyle(.link)
                }
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity)
                .padding(.top, 2)
            }
            .frame(maxWidth: 400)
        }
    }
}

private struct Feature: View {
    let systemImage: String
    let title: String
    let text: String

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: systemImage)
                .font(.system(size: 18))
                .foregroundStyle(Color.accentColor)
                .frame(width: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.system(size: 13, weight: .semibold))
                Text(text).font(.system(size: 12)).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

private struct OllamaStep: View {
    @Environment(ModelStore.self) private var store
    @State private var install = OllamaInstall.detect()

    var body: some View {
        StepLayout(systemImage: "shippingbox.fill", color: .indigo, title: "Connect to Ollama",
                   subtitle: "Local Router works on top of Ollama, which downloads and runs the models.") {
            VStack(spacing: 12) {
                CheckRow(title: "Ollama installed", detail: install.detail, state: install == .missing ? .failed : .passed)
                CheckRow(title: "Ollama running", detail: runningDetail, state: runningState)

                HStack(spacing: 10) {
                    if install == .missing {
                        Button("Download Ollama") {
                            NSWorkspace.shared.open(URL(string: "https://ollama.com/download")!)
                        }
                        .buttonStyle(.borderedProminent)
                    } else if store.status == .offline {
                        Button("Start Ollama") { store.startOllama() }
                            .buttonStyle(.borderedProminent)
                    }
                    if store.status != .running(version: runningVersion) || install == .missing {
                        Button("Check Again") { recheck() }
                    }
                }
                .padding(.top, 4)
            }
            .frame(maxWidth: 400)
        }
        .task {
            // Keep checking while this step is showing, so installing or
            // starting Ollama elsewhere is picked up.
            while !Task.isCancelled {
                recheck()
                try? await Task.sleep(for: .seconds(2))
            }
        }
    }

    private var runningVersion: String {
        if case .running(let version) = store.status { return version }
        return ""
    }

    private var runningState: CheckRow.State {
        switch store.status {
        case .running: .passed
        case .checking: .checking
        case .offline: .failed
        }
    }

    private var runningDetail: String {
        switch store.status {
        case .running(let version): "Version \(version) · \(store.installed.count) models"
        case .checking: "Checking…"
        case .offline: "Not responding at localhost:11434"
        }
    }

    private func recheck() {
        install = OllamaInstall.detect()
        Task { await store.refresh() }
    }
}

private struct StorageStep: View {
    var body: some View {
        StepLayout(systemImage: "externaldrive.fill", color: .teal, title: "Where Models Live",
                   subtitle: "Models are big. Keep them on an external drive if your Mac's disk is small.") {
            ModelsFolderView()
                .frame(maxWidth: 470)
        }
    }
}

private struct ServiceStep: View {
    @State private var daemon = DaemonStore.shared

    var body: some View {
        StepLayout(systemImage: "server.rack", color: .indigo, title: "Background Service",
                   subtitle: "Apps like Scribely use your models through Local Router's background service. It runs only on this Mac.") {
            VStack(alignment: .leading, spacing: 12) {
                ServiceRow()
                DriveAccessNote()
                    .padding(.horizontal, 12)
            }
            .frame(maxWidth: 440)
        }
        .task {
            // Reaching this step turns it on; macOS then shows its notice.
            daemon.ensureRegistered()
            while !Task.isCancelled {
                await daemon.refresh()
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }
}

private struct PermissionsStep: View {
    @Environment(ModelStore.self) private var store
    @State private var loginStatus = SMAppService.mainApp.status

    var body: some View {
        StepLayout(systemImage: "hand.raised.fill", color: .orange, title: "Permissions",
                   subtitle: "Two optional permissions. Local Router only talks to Ollama on this Mac, and never reads your files.") {
            VStack(spacing: 12) {
                NotificationsRow()
                PermissionRow(systemImage: "power", title: "Open at Login",
                              text: loginStatus == .requiresApproval ? "Waiting for your approval in Login Items."
                                                                     : "Keeps Local Router in your menu bar after a restart.",
                              state: loginState, actionTitle: loginAction) {
                    toggleLogin()
                }
            }
            .frame(maxWidth: 420)
        }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            loginStatus = SMAppService.mainApp.status
        }
    }

    private var loginState: CheckRow.State {
        switch loginStatus {
        case .enabled: .passed
        case .requiresApproval: .failed
        default: .pending
        }
    }

    private var loginAction: String? {
        switch loginStatus {
        case .enabled: nil
        case .requiresApproval: "Open Login Items"
        default: "Turn On"
        }
    }

    private func toggleLogin() {
        if loginStatus == .requiresApproval {
            SMAppService.openSystemSettingsLoginItems()
            return
        }
        do {
            try SMAppService.mainApp.register()
        } catch {
            store.lastError = "Open at Login: \(error.localizedDescription)"
        }
        loginStatus = SMAppService.mainApp.status
    }
}

private struct DoneStep: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        StepLayout(systemImage: "checkmark", color: .green, title: "You're All Set",
                   subtitle: "Local Router lives in your menu bar. Click its icon to see your models.") {
            Button {
                openWindow(id: "library")
            } label: {
                Label("Browse Models for My Mac", systemImage: "arrow.down.circle")
            }
            .controlSize(.large)
        }
    }
}

/// Where Ollama is installed, if anywhere.
enum OllamaInstall: Equatable {
    case app(String), cli(String), missing

    static func detect() -> OllamaInstall {
        if let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: "com.electron.ollama") {
            return .app(url.path)
        }
        for path in ["/opt/homebrew/bin/ollama", "/usr/local/bin/ollama"] where FileManager.default.isExecutableFile(atPath: path) {
            return .cli(path)
        }
        return .missing
    }

    var detail: String {
        switch self {
        case .app(let path): "Ollama app at \(path)"
        case .cli(let path): "Command-line Ollama at \(path)"
        case .missing: "Not found. Download it from ollama.com, then come back."
        }
    }
}
