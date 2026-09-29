import Foundation
import SwiftUI

/// How well a model of a given size runs on this Mac.
enum Fit: Int, Comparable {
    case tooBig, tight, good, great

    static func < (lhs: Fit, rhs: Fit) -> Bool { lhs.rawValue < rhs.rawValue }

    var label: String {
        switch self {
        case .great: "Great Fit"
        case .good: "Runs Well"
        case .tight: "Tight Fit"
        case .tooBig: "Too Big"
        }
    }

    var systemImage: String {
        switch self {
        case .great: "checkmark.seal.fill"
        case .good: "checkmark.circle.fill"
        case .tight: "exclamationmark.circle.fill"
        case .tooBig: "xmark.circle.fill"
        }
    }

    var color: Color {
        switch self {
        case .great: .green
        case .good: .blue
        case .tight: .orange
        case .tooBig: .secondary
        }
    }

    var explanation: String {
        switch self {
        case .great: "Plenty of room left for other models and apps."
        case .good: "Fits in GPU memory with room for a long context."
        case .tight: "Fits, but leaves little room. Close other apps first."
        case .tooBig: "Needs more memory than this Mac can give the GPU."
        }
    }
}

struct MachineInfo: Sendable {
    var modelName = "Mac"
    var chip = "Apple silicon"
    var memory = Int64(ProcessInfo.processInfo.physicalMemory)
    var gpuCores: Int?
    /// What macOS lets the GPU use (`iogpu.wired_limit_mb`, or the default share of RAM).
    var gpuMemory: Int64 = 0
    var freeDisk: Int64?
    var diskName: String?

    var symbol: String {
        let name = modelName.lowercased()
        if name.contains("studio") { return "macstudio" }
        if name.contains("mini") { return "macmini" }
        if name.contains("pro") && !name.contains("book") { return "macpro.gen3" }
        if name.contains("book") { return "laptopcomputer" }
        return "desktopcomputer"
    }

    /// Weights plus room for the KV cache and runtime.
    func fit(for size: Int64) -> Fit {
        let needed = Double(size) * 1.15 + 1.5e9
        let usable = Double(gpuMemory)
        switch needed {
        case ...(usable * 0.5): return .great
        case ...(usable * 0.85): return .good
        case ...usable: return .tight
        default: return .tooBig
        }
    }

    func hasDiskSpace(for size: Int64) -> Bool {
        guard let freeDisk else { return true }
        return freeDisk > size + 2_000_000_000
    }

    static func scan() async -> MachineInfo {
        await Task.detached(priority: .userInitiated) {
            var info = MachineInfo()
            if let hardware = profile("SPHardwareDataType")?.first {
                info.modelName = hardware["machine_name"] as? String ?? info.modelName
                info.chip = hardware["chip_type"] as? String ?? info.chip
            }
            if let display = profile("SPDisplaysDataType")?.first, let cores = display["sppci_cores"] as? String {
                info.gpuCores = Int(cores)
            }

            // macOS gives the GPU about 2/3 of RAM up to 36 GB and 3/4 above,
            // unless the user raised it with `sysctl iogpu.wired_limit_mb`.
            let limitMB = Int64(run("/usr/sbin/sysctl", ["-n", "iogpu.wired_limit_mb"]).trimmingCharacters(in: .whitespacesAndNewlines)) ?? 0
            let share = info.memory > 36 * 1_073_741_824 ? 0.75 : 0.67
            info.gpuMemory = limitMB > 0 ? limitMB * 1_048_576 : Int64(Double(info.memory) * share)

            let models = URL(fileURLWithPath: NSHomeDirectory()).appending(path: ".ollama/models").resolvingSymlinksInPath()
            if let values = try? models.resourceValues(forKeys: [.volumeAvailableCapacityForImportantUsageKey, .volumeNameKey]) {
                info.freeDisk = values.volumeAvailableCapacityForImportantUsage
                info.diskName = values.volumeName
            }
            return info
        }.value
    }

    private static func profile(_ type: String) -> [[String: Any]]? {
        let output = run("/usr/sbin/system_profiler", [type, "-json"])
        guard let data = output.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        return json[type] as? [[String: Any]]
    }

    private static func run(_ path: String, _ arguments: [String]) -> String {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: path)
        process.arguments = arguments
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        guard (try? process.run()) != nil else { return "" }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return String(decoding: data, as: UTF8.self)
    }
}
