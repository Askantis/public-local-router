import Foundation

/// "Keep a model loaded on a schedule": during the window, Local Router loads
/// the model (starting Ollama if needed) and pins it; afterwards it unpins it,
/// so it unloads when idle like any other model.
struct KeepLoadedSchedule: Codable, Equatable {
    var enabled = false
    /// nil means the default model.
    var model: String?
    /// Calendar weekdays, 1 = Sunday … 7 = Saturday. Monday to Friday by default.
    var days: Set<Int> = [2, 3, 4, 5, 6]
    /// Minutes after midnight.
    var start = 9 * 60
    var end = 18 * 60

    /// Windows can cross midnight (e.g. 22:00–02:00); the day is the start's.
    func isActive(at date: Date, calendar: Calendar = .current) -> Bool {
        guard enabled else { return false }
        let parts = calendar.dateComponents([.weekday, .hour, .minute], from: date)
        let minute = (parts.hour ?? 0) * 60 + (parts.minute ?? 0)
        let weekday = parts.weekday ?? 1
        if start <= end {
            return days.contains(weekday) && minute >= start && minute < end
        }
        let yesterday = weekday == 1 ? 7 : weekday - 1
        return (days.contains(weekday) && minute >= start) || (days.contains(yesterday) && minute < end)
    }

    static func load() -> KeepLoadedSchedule {
        guard let data = UserDefaults.standard.data(forKey: SettingsKey.schedule),
              let schedule = try? JSONDecoder().decode(KeepLoadedSchedule.self, from: data) else { return KeepLoadedSchedule() }
        return schedule
    }

    func save() {
        UserDefaults.standard.set(try? JSONEncoder().encode(self), forKey: SettingsKey.schedule)
    }
}

extension ModelStore {
    var scheduledModel: String? { schedule.model ?? defaultModel }

    /// Runs on every poll. Loads and pins at the start of the window, unpins at
    /// the end. Starting Ollama and loading are tried at most once a minute.
    func applySchedule() async {
        guard !applyingSchedule else { return }
        applyingSchedule = true
        defer { applyingSchedule = false }

        let active = schedule.isActive(at: .now)
        let model = scheduledModel

        // The window ended, or the scheduled model changed: release our pin.
        if let held = scheduleHolding, !active || held != model {
            scheduleHolding = nil
            if pinned.contains(held) {
                logStream.note("Schedule ended: \(ModelIdentity.prettyName(held)) unloads when idle again")
                await setKeepAlive(false, for: held)
            }
        }
        guard active, let model else { return }

        let retryOK = scheduleAttempt.map { Date.now.timeIntervalSince($0) > 60 } ?? true
        if status == .offline {
            if retryOK {
                scheduleAttempt = .now
                startOllama()
            }
            return
        }
        guard installed.contains(where: { $0.name == model }) else { return }
        if !loaded.contains(where: { $0.name == model }) {
            guard retryOK, pending[model] == nil else { return }
            scheduleAttempt = .now
            await load(model, manual: false)
        }
        guard loaded.contains(where: { $0.name == model }) else { return }
        // Only models the schedule pinned itself are unpinned when it ends;
        // a pin you set stays yours.
        if !pinned.contains(model) {
            logStream.note("Schedule: keeping \(ModelIdentity.prettyName(model)) loaded")
            await setKeepAlive(true, for: model)
            scheduleHolding = model
        }
    }
}
