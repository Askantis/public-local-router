import AppKit
import SwiftUI

/// A terminal-style view of what Ollama is doing: requests and where they
/// came from, prompt processing, generation speed, and Local Router's own events.
struct LogStreamView: View {
    @Environment(ModelStore.self) private var store
    @State private var shown: Set<LogLine.Kind> = [.request, .model, .connection, .app]
    @State private var hidePolling = true
    @State private var follow = true
    @State private var search = ""

    private var stream: LogStream { store.logStream }

    var body: some View {
        VStack(spacing: 0) {
            toolbar
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .background(.bar)
            Divider()
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 1) {
                        ForEach(visible) { line in
                            LogRow(line: line)
                                .id(line.id)
                        }
                    }
                    .padding(10)
                }
                .background(Color(nsColor: NSColor(white: 0.07, alpha: 1)))
                .onChange(of: stream.lines.last?.id) { _, last in
                    if follow, let last { proxy.scrollTo(last, anchor: .bottom) }
                }
                .onAppear {
                    if let last = visible.last?.id { proxy.scrollTo(last, anchor: .bottom) }
                }
            }
            Divider()
            statusBar
                .padding(.horizontal, 12)
                .padding(.vertical, 6)
                .background(.bar)
        }
        .frame(minWidth: 760, minHeight: 440)
        .windowHeader("Log Stream")
        .searchable(text: $search, placement: .toolbar, prompt: "Filter")
        .background(BringToFront())
        .onAppear { stream.start() }
        .onDisappear { stream.stop() }
    }

    private var visible: [LogLine] {
        let query = search.lowercased()
        return stream.lines.filter { line in
            shown.contains(line.kind) && !(hidePolling && line.isPolling)
                && (query.isEmpty || line.text.lowercased().contains(query) || line.source.contains(query))
        }
    }

    private var toolbar: some View {
        HStack(spacing: 6) {
            ForEach(LogLine.Kind.allCases) { kind in
                let on = shown.contains(kind)
                Button {
                    if on { shown.remove(kind) } else { shown.insert(kind) }
                } label: {
                    Text(kind.label)
                        .font(.system(size: 11, weight: .medium))
                        .padding(.horizontal, 9)
                        .padding(.vertical, 4)
                        .foregroundStyle(on ? AnyShapeStyle(.white) : AnyShapeStyle(.secondary))
                        .background(on ? AnyShapeStyle(Color.accentColor) : AnyShapeStyle(.quaternary), in: Capsule())
                }
                .buttonStyle(.plain)
                .pointerStyle(.link)
            }
            Spacer()
            Toggle("Hide Status Checks", isOn: $hidePolling)
                .toggleStyle(.checkbox)
                .font(.system(size: 11))
                .help("Hide the frequent model-list and status requests from Local Router and other apps")
            Toggle("Follow", isOn: $follow)
                .toggleStyle(.checkbox)
                .font(.system(size: 11))
            Button("Clear") { stream.clear() }
                .font(.system(size: 11))
        }
    }

    private var statusBar: some View {
        HStack(spacing: 10) {
            if stream.clients.isEmpty {
                Label("No apps connected", systemImage: "circle.dotted")
                    .foregroundStyle(.secondary)
            } else {
                Label("Connected: " + stream.clients.map(\.name).joined(separator: ", "), systemImage: "point.3.connected.trianglepath.dotted")
                    .foregroundStyle(.green)
            }
            Spacer()
            if let path = stream.logPath {
                Button {
                    NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
                } label: {
                    Text(path)
                        .lineLimit(1)
                        .truncationMode(.head)
                        .foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
                .pointerStyle(.link)
                .help("Show Ollama's log file in Finder")
            }
        }
        .font(.system(size: 11))
    }
}

private struct LogRow: View {
    let line: LogLine

    private static let time: DateFormatter = {
        let formatter = DateFormatter()
        formatter.dateFormat = "HH:mm:ss"
        return formatter
    }()

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(Self.time.string(from: line.date))
                .foregroundStyle(Color(white: 0.45))
            Text(line.source)
                .foregroundStyle(sourceColor)
                .frame(width: 104, alignment: .leading)
                .lineLimit(1)
            Text(line.text)
                .foregroundStyle(textColor)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(.system(size: 11.5, design: .monospaced))
    }

    private var sourceColor: Color {
        switch line.source {
        case "ollama": Color(red: 0.55, green: 0.7, blue: 1)
        case "local router": Color(red: 0.7, green: 0.6, blue: 1)
        case "hermes": Color(red: 1, green: 0.75, blue: 0.4)
        default: line.source.first?.isNumber == true ? .orange : Color(red: 0.5, green: 0.9, blue: 0.8)
        }
    }

    private var textColor: Color {
        switch line.tone {
        case .normal: Color(white: 0.88)
        case .dim: Color(white: 0.5)
        case .good: Color(red: 0.45, green: 0.85, blue: 0.5)
        case .busy: Color(red: 1, green: 0.72, blue: 0.35)
        case .warning: .yellow
        case .error: Color(red: 1, green: 0.4, blue: 0.4)
        }
    }
}
