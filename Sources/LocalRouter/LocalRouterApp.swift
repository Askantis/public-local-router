import AppKit
import SwiftUI
import UserNotifications

@main
struct LocalRouterApp: App {
    @NSApplicationDelegateAdaptor private var appDelegate: AppDelegate
    @State private var store = ModelStore()
    @AppStorage(SettingsKey.onboardingDone) private var onboardingDone = false
    @AppStorage(SettingsKey.menuBarMemory) private var showMemory = false
    @AppStorage(SettingsKey.menuBarContext) private var showContext = true
    @AppStorage(SettingsKey.menuBarUptime) private var showUptime = false

    var body: some Scene {
        MenuBarExtra {
            MenuContentView()
                .environment(store)
        } label: {
            // One image only: the menu bar drops a second one, so the activity
            // dot (orange reading, pink generating) is drawn into the mark.
            Image(nsImage: .localRouterMark(badge: store.overallActivity.color))
            if let text = store.menuBarText(memory: showMemory, context: showContext, uptime: showUptime) {
                Text(text)
            }
        }
        .menuBarExtraStyle(.window)

        Window("Download Models", id: "library") {
            LibraryView()
                .environment(store)
        }
        .defaultSize(width: 900, height: 700)
        .handlesExternalEvents(matching: ["library"])
        .defaultLaunchBehavior(.suppressed)
        .restorationBehavior(.disabled)

        Window("Settings", id: "settings") {
            SettingsView()
                .environment(store)
        }
        .defaultSize(width: 780, height: 620)
        .handlesExternalEvents(matching: ["settings", "advanced"])
        .defaultLaunchBehavior(.suppressed)
        .restorationBehavior(.disabled)

        Window("Log Stream", id: "logs") {
            LogStreamView()
                .environment(store)
        }
        .defaultSize(width: 900, height: 560)
        .handlesExternalEvents(matching: ["logs"])
        .defaultLaunchBehavior(.suppressed)
        .restorationBehavior(.disabled)

        WindowGroup("Model Settings", id: "model", for: String.self) { $model in
            if let model {
                ModelSettingsView(modelName: model)
                    .environment(store)
            }
        }
        .windowResizability(.contentSize)
        .defaultLaunchBehavior(.suppressed)
        .restorationBehavior(.disabled)

        Window("Models Folder", id: "storage") {
            ModelsFolderWindow()
                .environment(store)
        }
        .windowResizability(.contentSize)
        .handlesExternalEvents(matching: ["storage"])
        .defaultLaunchBehavior(.suppressed)
        .restorationBehavior(.disabled)

        Window("Welcome to Local Router", id: "onboarding") {
            OnboardingView()
                .environment(store)
        }
        .windowStyle(.hiddenTitleBar)
        .handlesExternalEvents(matching: ["welcome"])
        .windowResizability(.contentSize)
        .defaultLaunchBehavior(onboardingDone ? .suppressed : .presented)
        .restorationBehavior(.disabled)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate, UNUserNotificationCenterDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // No Dock icon, also when run with `swift run` outside the app bundle.
        NSApp.setActivationPolicy(.accessory)
        NotificationIDs.register()
        UNUserNotificationCenter.current().delegate = self
    }

    /// Show notifications even while the menu is open.
    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async
        -> UNNotificationPresentationOptions { [.banner, .sound] }

    /// "Make Default" on the suggestion notification.
    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        guard response.actionIdentifier == NotificationIDs.makeDefault,
              let model = response.notification.request.content.userInfo["model"] as? String else { return }
        await MainActor.run { ModelStore.current?.defaultModel = model }
    }
}

