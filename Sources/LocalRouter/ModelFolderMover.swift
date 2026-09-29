import Foundation

/// File work for changing where Ollama keeps models. Ollama reads
/// `~/.ollama/models`, so the chosen folder is linked there with a symlink:
/// that works for Ollama.app, Homebrew and `ollama serve` alike, unless
/// `OLLAMA_MODELS` overrides it.
enum ModelFolderMover {
    static var linkPath: URL { URL(fileURLWithPath: NSHomeDirectory()).appending(path: ".ollama/models") }

    struct MoveError: LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    /// A folder already holding Ollama models (it has a `manifests` folder).
    static func containsModels(_ folder: URL) -> Bool {
        FileManager.default.fileExists(atPath: folder.appending(path: "manifests").path)
    }

    static func sameVolume(_ a: URL, _ b: URL) -> Bool {
        let key: Set<URLResourceKey> = [.volumeIdentifierKey]
        guard let first = try? a.resourceValues(forKeys: key).volumeIdentifier as? NSObject,
              let second = try? b.resourceValues(forKeys: key).volumeIdentifier as? NSObject else { return false }
        return first.isEqual(second)
    }

    static func size(of folder: URL) -> Int64 {
        guard let files = FileManager.default.enumerator(at: folder, includingPropertiesForKeys: [.fileSizeKey, .isRegularFileKey]) else { return 0 }
        var total: Int64 = 0
        for case let file as URL in files {
            let values = try? file.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
            if values?.isRegularFile == true { total += Int64(values?.fileSize ?? 0) }
        }
        return total
    }

    /// Moves `blobs` then `manifests` from `source` to `destination`. On the same
    /// drive that's a rename; across drives it copies, checks every file's size,
    /// and only then deletes the originals. Blobs go first because Ollama deletes
    /// blobs that no manifest points to.
    static func transfer(from source: URL, to destination: URL, progress: @Sendable (Double) -> Void) throws {
        let fileManager = FileManager.default
        try fileManager.createDirectory(at: destination, withIntermediateDirectories: true)
        let parts = ["blobs", "manifests"].map { source.appending(path: $0) }.filter { fileManager.fileExists(atPath: $0.path) }

        if sameVolume(source, destination) {
            for part in parts {
                try merge(part, into: destination.appending(path: part.lastPathComponent))
            }
            progress(1)
            return
        }

        // Every file with its size, blobs first.
        var files: [(URL, Int64)] = []
        for part in parts {
            guard let enumerator = fileManager.enumerator(at: part, includingPropertiesForKeys: [.fileSizeKey, .isRegularFileKey]) else { continue }
            for case let file as URL in enumerator {
                let values = try file.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
                if values.isRegularFile == true { files.append((file, Int64(values.fileSize ?? 0))) }
            }
        }
        let total = max(1, files.reduce(0) { $0 + $1.1 })
        var copied: Int64 = 0

        for (file, size) in files {
            let relative = String(file.path.dropFirst(source.path.count + 1))
            let target = destination.appending(path: relative)
            try fileManager.createDirectory(at: target.deletingLastPathComponent(), withIntermediateDirectories: true)
            if let existing = try? target.resourceValues(forKeys: [.fileSizeKey]).fileSize, Int64(existing) == size {
                copied += size // already there, e.g. from an earlier attempt
                progress(Double(copied) / Double(total))
                continue
            }
            try copyFile(file, to: target) { chunk in
                copied += chunk
                progress(Double(copied) / Double(total))
            }
            let written = Int64((try? target.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? -1)
            guard written == size else {
                throw MoveError(message: "Copy check failed for \(relative). Nothing was deleted.")
            }
        }

        for part in parts {
            try fileManager.removeItem(at: part)
        }
    }

    /// Makes `~/.ollama/models` point at `destination`. A real folder that still
    /// has models in it is kept, renamed with a date, never deleted.
    static func repoint(link: URL = linkPath, to destination: URL) throws {
        let fileManager = FileManager.default
        if destination.standardizedFileURL.path == link.standardizedFileURL.path { return }
        try fileManager.createDirectory(at: link.deletingLastPathComponent(), withIntermediateDirectories: true)

        // Ask the file system, not URL resource values: those are cached per URL
        // and can still say "symlink" after the link was replaced by a folder.
        let isLink = (try? fileManager.destinationOfSymbolicLink(atPath: link.path)) != nil
        if isLink {
            try fileManager.removeItem(at: link)
        } else if fileManager.fileExists(atPath: link.path) {
            let leftovers = (try? fileManager.contentsOfDirectory(atPath: link.path).filter { $0 != ".DS_Store" }) ?? []
            if leftovers.isEmpty {
                try fileManager.removeItem(at: link)
            } else {
                let stamp = ISO8601DateFormatter.dateOnly.string(from: .now)
                try fileManager.moveItem(at: link, to: link.deletingLastPathComponent().appending(path: "models-before-\(stamp)"))
            }
        }
        try fileManager.createSymbolicLink(at: link, withDestinationURL: destination)
    }

    // MARK: - Helpers

    /// Moves a folder into place, merging file by file when the target exists.
    private static func merge(_ source: URL, into target: URL) throws {
        let fileManager = FileManager.default
        guard fileManager.fileExists(atPath: target.path) else {
            try fileManager.moveItem(at: source, to: target)
            return
        }
        for name in try fileManager.contentsOfDirectory(atPath: source.path) {
            let item = source.appending(path: name)
            let destination = target.appending(path: name)
            var isDirectory: ObjCBool = false
            fileManager.fileExists(atPath: item.path, isDirectory: &isDirectory)
            if isDirectory.boolValue {
                try merge(item, into: destination)
            } else {
                if fileManager.fileExists(atPath: destination.path) { try fileManager.removeItem(at: destination) }
                try fileManager.moveItem(at: item, to: destination)
            }
        }
        try fileManager.removeItem(at: source)
    }

    /// Copies in 16 MB chunks so progress moves smoothly through big blobs.
    private static func copyFile(_ source: URL, to target: URL, chunk: (Int64) -> Void) throws {
        FileManager.default.createFile(atPath: target.path, contents: nil)
        let input = try FileHandle(forReadingFrom: source)
        let output = try FileHandle(forWritingTo: target)
        defer {
            try? input.close()
            try? output.close()
        }
        while let data = try input.read(upToCount: 16 * 1024 * 1024), !data.isEmpty {
            try output.write(contentsOf: data)
            chunk(Int64(data.count))
        }
    }
}
