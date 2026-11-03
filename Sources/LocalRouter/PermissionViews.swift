import AppKit
import ServiceManagement
import SwiftUI
import UserNotifications

// Rows for checks and permissions, shared by the welcome guide and Settings,
// and the two permissions Local Router asks for itself. The words follow
// lib-desktop-kit/docs/PERMISSIONS.md: say what it is for, ask only from a
// button, and send people to the exact System Settings pane when denied.

struct CheckRow: View {
    enum State { case passed, failed, pending, checking }

    let title: String
    let detail: String
    let state: State

    var body: some View {
        HStack(spacing: 12) {
            StateIcon(state: state)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.system(size: 13, weight: .semibold))
                Text(detail).font(.system(size: 12)).foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(12)
        .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}

struct PermissionRow: View {
    let systemImage: String
    let title: String
    let text: String
    let state: CheckRow.State
    let actionTitle: String?
    let action: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: systemImage)
                .font(.system(size: 16))
                .foregroundStyle(Color.accentColor)
                .frame(width: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.system(size: 13, weight: .semibold))
                Text(text).font(.system(size: 12)).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer()
            if let actionTitle {
                Button(actionTitle, action: action)
                    .buttonBorderShape(.capsule)
            } else {
                StateIcon(state: state)
            }
        }
        .padding(12)
        .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}

struct StateIcon: View {
    let state: CheckRow.State

    var body: some View {
        switch state {
        case .passed:
            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green).font(.system(size: 18))
        case .failed:
            Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.orange).font(.system(size: 18))
        case .pending:
            Image(systemName: "circle").foregroundStyle(.tertiary).font(.system(size: 18))
        case .checking:
            ProgressView().controlSize(.small).frame(width: 18)
        }
    }
}


// MARK: - Notifications

/// Notifications: state, and the one thing to do (Allow, or Open Settings).
struct NotificationsRow: View {
    @State private var status: UNAuthorizationStatus = .notDetermined

    var body: some View {
        PermissionRow(systemImage: "bell.badge", title: "Notifications",
                      text: status == .denied ? "Turned off in System Settings. Local Router can't tell you when a download finishes."
                                              : "Tells you when a model download finishes.",
                      state: state, actionTitle: actionTitle) {
            act()
        }
        .task { await refresh() }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            Task { await refresh() }
        }
    }

    private func refresh() async {
        status = await UNUserNotificationCenter.current().notificationSettings().authorizationStatus
    }

    private var state: CheckRow.State {
        switch status {
        case .authorized, .provisional, .ephemeral: .passed
        case .denied: .failed
        default: .pending
        }
    }

    private var actionTitle: String? {
        switch status {
        case .notDetermined: "Allow"
        case .denied: "Open Settings"
        default: nil
        }
    }

    private func act() {
        if status == .denied {
            NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension?id=ch.askantis.localrouter")!)
            return
        }
        Task {
            NSApp.activate()
            _ = try? await UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound])
            await refresh()
        }
    }
}

// MARK: - Background service

/// The background service apps use: running, or what's in the way.
struct ServiceRow: View {
    @State private var daemon = DaemonStore.shared

    var body: some View {
        PermissionRow(systemImage: "server.rack", title: "Background service",
                      text: text, state: state, actionTitle: actionTitle) {
            act()
        }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            Task { await daemon.refresh() }
        }
    }

    private var state: CheckRow.State {
        if daemon.running { return .passed }
        switch daemon.serviceStatus {
        case .enabled: return .checking
        case .requiresApproval, .notFound: return .failed
        default: return .pending
        }
    }

    private var text: String {
        if daemon.running { return "Running. Apps on this Mac can use your models." }
        switch daemon.serviceStatus {
        case .enabled: return "Starting…"
        case .requiresApproval: return "Turned off in Login Items. Turn on Local Router under “Allow in the Background”."
        case .notFound: return "macOS can't find the service. Move Local Router to the Applications folder and open it again."
        default: return "Lets apps like Scribely use your models, also when the menu is closed. macOS shows a notice that a background item was added."
        }
    }

    private var actionTitle: String? {
        if daemon.running { return nil }
        switch daemon.serviceStatus {
        case .notRegistered: return "Turn On"
        case .requiresApproval: return "Open Login Items"
        default: return nil
        }
    }

    private func act() {
        if daemon.serviceStatus == .requiresApproval {
            daemon.openLoginItems()
        } else {
            daemon.ensureRegistered()
        }
    }
}

// MARK: - Speech models folder

/// The drive or folder with the speech models, when the service can't use it.
struct DriveAccessNote: View {
    @State private var daemon = DaemonStore.shared

    var body: some View {
        if let path = daemon.hfCachePath {
            if daemon.hfCacheState == "not_allowed" {
                note("Local Router may not open \(DriveAccessNote.place(path)), where your speech models are. Allow it under Privacy & Security → Files & Folders → Local Router (“Removable Volumes” or “Network Volumes”), then check again.",
                     settings: true)
            } else if daemon.waitingForDrivePermission {
                note("Your speech models are on \(DriveAccessNote.place(path)). macOS asks before a background service reads a drive like this: click Allow if it asked.",
                     settings: true)
            } else if daemon.hfCacheState == "missing_volume" {
                note("The drive “\(DriveAccessNote.volumeName(path))” with your speech models isn't connected. Connect it; Local Router notices by itself.",
                     settings: false)
            }
        }
    }

    private func note(_ text: String, settings: Bool) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(text).font(.callout).fixedSize(horizontal: false, vertical: true)
            HStack {
                if settings {
                    Button("Open Files & Folders…") { daemon.openFilesAndFoldersSettings() }
                }
                Button("Check Again") { Task { await daemon.checkDriveAgain() } }
            }
        }
    }

    /// "/Volumes/Models/…" → "Models"; anything else → the folder's name.
    static func volumeName(_ path: String) -> String {
        let parts = URL(fileURLWithPath: path).pathComponents
        if parts.count > 2, parts[1] == "Volumes" { return parts[2] }
        return URL(fileURLWithPath: path).lastPathComponent
    }

    static func place(_ path: String) -> String {
        let parts = URL(fileURLWithPath: path).pathComponents
        return parts.count > 2 && parts[1] == "Volumes" ? "the drive “\(parts[2])”" : "the folder “\(URL(fileURLWithPath: path).lastPathComponent)”"
    }
}
