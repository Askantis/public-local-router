import SwiftUI

/// "Download Models": checks this Mac, then lists Ollama library models,
/// with the ones that run well here first.
struct LibraryView: View {
    @Environment(ModelStore.self) private var store
    @State private var machine: MachineInfo?
    @State private var forMyMac = true
    @State private var uses: Set<ModelTag> = []
    @State private var capabilities: Set<Capability> = []
    @State private var formats: Set<ModelFormat> = []
    @State private var search = ""
    @AppStorage(SettingsKey.preferMLX) private var preferMLX = SettingsKey.preferMLXDefault
    /// Download size range in GB. The upper end starts at the Mac's RAM.
    @State private var minSize: Double = 0
    @State private var maxSize: Double?

    var body: some View {
        VStack(spacing: 0) {
            MachineHeader(machine: machine)
                .padding(.horizontal, 20)
                .padding(.top, 16)
                .padding(.bottom, 12)

            filterBar
                .padding(.horizontal, 20)
                .padding(.bottom, 10)

            if let machine {
                HStack(spacing: 16) {
                    SizeFilter(minSize: $minSize, maxSize: Binding(get: { maxSize ?? ramGB(machine) }, set: { maxSize = $0 }),
                               bounds: 0...sizeBound(machine), defaultMax: ramGB(machine))
                    Toggle("Prefer MLX", isOn: $preferMLX)
                        .toggleStyle(.switch)
                        .controlSize(.mini)
                        .font(.system(size: 12))
                        .fixedSize()
                        .help("Suggest MLX builds when a model has one. MLX is Apple's framework for Apple silicon.")
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 12)
            }

            Divider()

            if results.isEmpty {
                ContentUnavailableView(
                    "No Matching Models",
                    systemImage: "square.stack.3d.up.slash",
                    description: Text(forMyMac ? "Try turning off For My Mac or removing a filter." : "Try removing a filter.")
                )
            } else {
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(results) { model in
                            CatalogRow(model: model, machine: machine, format: formats.count == 1 ? formats.first : nil, sizeRange: sizeRange)
                            Divider().padding(.leading, 78)
                        }
                    }
                    .padding(.bottom, 8)
                }
            }
        }
        .searchable(text: $search, placement: .toolbar, prompt: "Search models")
        .windowHeader("Download Models", detail: "\(results.count) models" + (Catalog.generatedAt.map { " · library from \($0)" } ?? ""))
        .frame(minWidth: 780, minHeight: 560)
        .background(BringToFront())
        .task {
            NSApp.activate()
            if machine == nil { machine = await MachineInfo.scan() }
        }
    }

    /// For My Mac, then format, then capabilities, then a "Good For" menu.
    private var filterBar: some View {
        FlowLayout(spacing: 6, lineSpacing: 8) {
            FilterChip(label: "For My Mac", systemImage: "checkmark.seal", color: .accentColor, isOn: $forMyMac)
            FilterSeparator()
            ForEach(ModelFormat.allCases) { format in
                FilterChip(label: format.label, systemImage: format.systemImage, color: format.color,
                           isOn: binding(format, in: $formats))
            }
            FilterSeparator()
            ForEach(Capability.allCases) { capability in
                FilterChip(label: capability.label, systemImage: capability.systemImage, color: capability.color,
                           isOn: binding(capability, in: $capabilities))
            }
            FilterSeparator()
            GoodForMenu(selection: $uses)
        }
    }

    private var sizeRange: ClosedRange<Int64>? {
        guard let machine else { return nil }
        let upper = maxSize ?? ramGB(machine)
        return Int64(minSize * 1e9)...Int64(max(minSize, upper) * 1e9)
    }

    private func ramGB(_ machine: MachineInfo) -> Double {
        (Double(machine.memory) / 1_073_741_824).rounded()
    }

    /// Room to look past this Mac's RAM, up to twice as much.
    private func sizeBound(_ machine: MachineInfo) -> Double {
        max(64, ramGB(machine) * 2)
    }

    private var results: [CatalogModel] {
        let query = search.trimmingCharacters(in: .whitespaces).lowercased()
        let format = formats.count == 1 ? formats.first : nil
        return Catalog.models
            .filter { model in
                (query.isEmpty || model.name.contains(query) || model.title.lowercased().contains(query)
                    || model.description.lowercased().contains(query))
                    && uses.isSubset(of: Set(model.useList))
                    && capabilities.isSubset(of: Set(model.capabilityList))
                    && formats.isSubset(of: Set(model.formats))
                    && model.bestVariant(on: nil, format: format, sizeRange: sizeRange, preferMLX: preferMLX) != nil
            }
            .filter { model in
                guard forMyMac, let machine, let best = model.bestVariant(on: machine, format: format, sizeRange: sizeRange, preferMLX: preferMLX) else { return true }
                return machine.fit(for: best.size) >= .good
            }
            .sorted { $0.score > $1.score }
    }

    private func binding<T: Hashable>(_ value: T, in set: Binding<Set<T>>) -> Binding<Bool> {
        Binding(
            get: { set.wrappedValue.contains(value) },
            set: { isOn in
                if isOn { set.wrappedValue.insert(value) } else { set.wrappedValue.remove(value) }
            }
        )
    }
}

// MARK: - Header

private struct MachineHeader: View {
    let machine: MachineInfo?

    var body: some View {
        HStack(spacing: 14) {
            ZStack {
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .fill(.quaternary.opacity(0.6))
                Image(systemName: machine?.symbol ?? "desktopcomputer")
                    .font(.system(size: 24, weight: .light))
                    .foregroundStyle(.secondary)
            }
            .frame(width: 48, height: 48)

            if let machine {
                VStack(alignment: .leading, spacing: 3) {
                    Text(machine.modelName)
                        .font(.system(size: 15, weight: .semibold))
                    Text(specs(machine))
                        .font(.system(size: 12))
                        .foregroundStyle(.secondary)
                }
                Spacer()
                HStack(spacing: 8) {
                    StatPill(value: Format.bytes(Int64(Double(machine.gpuMemory) * 0.85)),
                             label: "for models",
                             help: "How big a model can be and still run well, with room for a long context. macOS lets the GPU use \(Format.ram(machine.gpuMemory)).")
                    if let free = machine.freeDisk {
                        StatPill(value: Format.bytes(free), label: "free on \(machine.diskName ?? "disk")",
                                 help: "Space left where Ollama stores models.")
                    }
                }
            } else {
                VStack(alignment: .leading, spacing: 3) {
                    Text("Checking Your Mac…")
                        .font(.system(size: 15, weight: .semibold))
                    Text("Reading chip, memory and free disk space.")
                        .font(.system(size: 12))
                        .foregroundStyle(.secondary)
                }
                Spacer()
                ProgressView().controlSize(.small)
            }
        }
        .padding(12)
        .background(.quaternary.opacity(0.35), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
    }

    private func specs(_ machine: MachineInfo) -> String {
        var parts = [machine.chip, "\(Format.ram(machine.memory)) memory"]
        if let cores = machine.gpuCores { parts.append("\(cores)-core GPU") }
        return parts.joined(separator: " · ")
    }
}

private struct StatPill: View {
    let value: String
    let label: String
    let help: String

    var body: some View {
        VStack(alignment: .trailing, spacing: 1) {
            Text(value)
                .font(.system(size: 13, weight: .semibold))
                .monospacedDigit()
            Text(label)
                .font(.system(size: 10))
                .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .background(.background.opacity(0.6), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
        .help(help)
    }
}

// MARK: - Filters

private struct FilterChip: View {
    let label: String
    let systemImage: String
    let color: Color
    @Binding var isOn: Bool

    var body: some View {
        Button {
            withAnimation(.snappy(duration: 0.15)) { isOn.toggle() }
        } label: {
            HStack(spacing: 4) {
                Image(systemName: isOn ? "\(systemImage)" : systemImage)
                    .font(.system(size: 10, weight: .semibold))
                Text(label)
                    .font(.system(size: 12, weight: .medium))
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 5)
            .foregroundStyle(isOn ? AnyShapeStyle(.white) : AnyShapeStyle(color))
            .background(isOn ? AnyShapeStyle(color) : AnyShapeStyle(color.opacity(0.12)), in: Capsule())
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .pointerStyle(.link)
    }
}

private struct FilterSeparator: View {
    var body: some View {
        Rectangle()
            .fill(.quaternary)
            .frame(width: 1, height: 16)
            .padding(.horizontal, 2)
            .padding(.vertical, 5)
    }
}

/// "Good For ▾": a menu of checkable uses, shown as a chip.
private struct GoodForMenu: View {
    @Binding var selection: Set<ModelTag>

    var body: some View {
        Menu {
            ForEach(ModelTag.allCases) { tag in
                Toggle(isOn: Binding(
                    get: { selection.contains(tag) },
                    set: { isOn in if isOn { selection.insert(tag) } else { selection.remove(tag) } }
                )) {
                    Label(tag.label, systemImage: tag.systemImage)
                }
            }
            if !selection.isEmpty {
                Divider()
                Button("Clear") { selection.removeAll() }
            }
        } label: {
            HStack(spacing: 4) {
                Image(systemName: "line.3.horizontal.decrease")
                    .font(.system(size: 10, weight: .semibold))
                Text(title)
                    .font(.system(size: 12, weight: .medium))
                Image(systemName: "chevron.down")
                    .font(.system(size: 8, weight: .bold))
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 5)
            .foregroundStyle(selection.isEmpty ? AnyShapeStyle(.primary) : AnyShapeStyle(.white))
            .background(selection.isEmpty ? AnyShapeStyle(.quaternary) : AnyShapeStyle(Color.accentColor), in: Capsule())
            .contentShape(Capsule())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .fixedSize()
        .pointerStyle(.link)
    }

    private var title: String {
        let chosen = ModelTag.allCases.filter(selection.contains)
        switch chosen.count {
        case 0: return "Good For"
        case 1, 2: return "Good For: " + chosen.map(\.label).joined(separator: ", ")
        default: return "Good For: \(chosen.count) uses"
        }
    }
}

/// "Size  0 GB ——●———●—— 64 GB": the download size range to show.
/// Dragging only moves the labels; the list is filtered when you let go,
/// so the slider stays smooth.
private struct SizeFilter: View {
    @Binding var minSize: Double
    @Binding var maxSize: Double
    let bounds: ClosedRange<Double>
    let defaultMax: Double

    @State private var draftMin: Double?
    @State private var draftMax: Double?

    var body: some View {
        let lower = draftMin ?? minSize
        let upper = draftMax ?? maxSize
        HStack(spacing: 12) {
            Text("Size")
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(.secondary)
            Text("\(Int(lower)) GB")
                .font(.system(size: 12, weight: .medium))
                .monospacedDigit()
                .frame(width: 48, alignment: .trailing)
            RangeSlider(lower: lower, upper: upper, bounds: bounds) { newLower, newUpper in
                draftMin = newLower
                draftMax = newUpper
            } onEnded: {
                if let draftMin { minSize = draftMin }
                if let draftMax { maxSize = draftMax }
                draftMin = nil
                draftMax = nil
            }
            .frame(height: 22)
            Text("\(Int(upper)) GB")
                .font(.system(size: 12, weight: .medium))
                .monospacedDigit()
                .frame(width: 52, alignment: .leading)
            Button("Reset") {
                minSize = 0
                maxSize = defaultMax
            }
            .buttonStyle(.borderless)
            .font(.system(size: 12))
            .opacity(minSize == 0 && maxSize == defaultMax ? 0 : 1)
            .help("Back to 0 – \(Int(defaultMax)) GB, this Mac's memory")
        }
    }
}

/// A two-thumb slider. The whole track is draggable: a drag moves the
/// nearer thumb. The scale is square-root, so the small sizes most models
/// have get more room than the rare huge ones.
private struct RangeSlider: View {
    let lower: Double
    let upper: Double
    let bounds: ClosedRange<Double>
    let onChange: (Double, Double) -> Void
    let onEnded: () -> Void

    private enum Thumb { case lower, upper }
    @State private var active: Thumb?
    private let thumb: CGFloat = 18

    var body: some View {
        GeometryReader { geometry in
            let width = max(1, geometry.size.width - thumb)
            let lowerX = position(lower) * width
            let upperX = position(upper) * width

            ZStack(alignment: .leading) {
                Capsule()
                    .fill(.quaternary)
                    .frame(height: 4)
                    .padding(.horizontal, thumb / 2)
                Capsule()
                    .fill(Color.accentColor)
                    .frame(width: max(0, upperX - lowerX), height: 4)
                    .offset(x: lowerX + thumb / 2)
                knob(isActive: active == .lower).offset(x: lowerX)
                knob(isActive: active == .upper).offset(x: upperX)
            }
            .frame(width: geometry.size.width, height: geometry.size.height)
            .contentShape(Rectangle())
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { drag in
                        let fraction = min(1, max(0, (drag.location.x - thumb / 2) / width))
                        if active == nil {
                            let toLower = abs(fraction - position(lower))
                            let toUpper = abs(fraction - position(upper))
                            // On a tie (thumbs together) pick by which side you grabbed.
                            active = toLower < toUpper || (toLower == toUpper && fraction < position(lower)) ? .lower : .upper
                        }
                        let picked = value(at: fraction).rounded()
                        switch active {
                        case .lower: onChange(min(picked, upper - 1), upper)
                        case .upper: onChange(lower, max(picked, lower + 1))
                        case nil: break
                        }
                    }
                    .onEnded { _ in
                        active = nil
                        onEnded()
                    }
            )
            .pointerStyle(active == nil ? .grabIdle : .grabActive)
        }
    }

    private func position(_ value: Double) -> CGFloat {
        let span = bounds.upperBound - bounds.lowerBound
        return CGFloat(sqrt(max(0, min(1, (value - bounds.lowerBound) / span))))
    }

    private func value(at fraction: CGFloat) -> Double {
        bounds.lowerBound + Double(fraction * fraction) * (bounds.upperBound - bounds.lowerBound)
    }

    private func knob(isActive: Bool) -> some View {
        Circle()
            .fill(.white)
            .frame(width: thumb, height: thumb)
            .shadow(color: .black.opacity(isActive ? 0.35 : 0.25), radius: isActive ? 3 : 1.5, y: 0.5)
            .overlay(Circle().strokeBorder(.black.opacity(0.08), lineWidth: 0.5))
            .scaleEffect(isActive ? 1.12 : 1)
            .animation(.snappy(duration: 0.12), value: isActive)
            .allowsHitTesting(false)
    }
}

// MARK: - Row

private struct CatalogRow: View {
    @Environment(ModelStore.self) private var store
    let model: CatalogModel
    let machine: MachineInfo?
    let format: ModelFormat?
    let sizeRange: ClosedRange<Int64>?
    @AppStorage(SettingsKey.preferMLX) private var preferMLX = SettingsKey.preferMLXDefault
    @State private var selectedTag: String?

    var body: some View {
        HStack(alignment: .top, spacing: 14) {
            ModelIcon(maker: model.maker, name: model.name, size: 44)

            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 8) {
                    Text(model.title)
                        .font(.system(size: 14, weight: .semibold))
                    if let machine, let variant {
                        let fit = machine.fit(for: variant.size)
                        Label(fit.label, systemImage: fit.systemImage)
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(fit.color)
                            .help(fit.explanation)
                    }
                }
                Text(model.description)
                    .font(.system(size: 12))
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .fixedSize(horizontal: false, vertical: true)
                FlowLayout(spacing: 5, lineSpacing: 5) {
                    ForEach(model.capabilityList) { Chip(label: $0.label, systemImage: $0.systemImage, color: $0.color) }
                    ForEach(model.formats) { Chip(label: $0.label, systemImage: $0.systemImage, color: $0.color) }
                }
                .padding(.top, 2)
                if !model.useList.isEmpty {
                    HStack(spacing: 10) {
                        Text("Good for")
                            .foregroundStyle(.tertiary)
                        ForEach(model.useList) { use in
                            Label(use.label, systemImage: use.systemImage)
                                .labelStyle(.titleAndIcon)
                                .foregroundStyle(.secondary)
                        }
                    }
                    .font(.system(size: 11))
                }
            }

            Spacer(minLength: 12)

            VStack(alignment: .trailing, spacing: 8) {
                if let variant {
                    variantPicker(selected: variant)
                    DownloadControl(pullName: model.pullName(variant), variant: variant, machine: machine)
                    if let other = otherFormatInstalled(than: variant) {
                        Text("\(other.mlx ? "MLX" : "GGUF") build installed")
                            .font(.system(size: 10))
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .frame(width: 190, alignment: .trailing)
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 14)
    }

    private var variant: CatalogVariant? {
        if let selectedTag, let chosen = model.variants.first(where: { $0.tag == selectedTag }) { return chosen }
        return model.bestVariant(on: machine, format: format, sizeRange: sizeRange, preferMLX: preferMLX)
    }

    /// The same size in the other format, if that one's installed, so a model
    /// you already have in GGUF doesn't look missing when MLX is suggested.
    private func otherFormatInstalled(than variant: CatalogVariant) -> CatalogVariant? {
        guard !store.isInstalled(model.pullName(variant), isDefault: variant.isDefault) else { return nil }
        return model.variants.first { other in
            other.mlx != variant.mlx && other.sizeLabel == variant.sizeLabel
                && store.isInstalled(model.pullName(other), isDefault: other.isDefault)
        }
    }

    private func variantPicker(selected: CatalogVariant) -> some View {
        Menu {
            ForEach(model.variants) { option in
                Button {
                    selectedTag = option.tag
                } label: {
                    let fit = machine?.fit(for: option.size)
                    Label(label(for: option), systemImage: fit?.systemImage ?? "circle")
                }
            }
        } label: {
            Text(label(for: selected))
                .font(.system(size: 12))
                .monospacedDigit()
        }
        .menuStyle(.borderlessButton)
        .fixedSize()
        .pointerStyle(.link)
        .help("Choose a size")
    }

    private func label(for variant: CatalogVariant) -> String {
        var text = "\(variant.sizeLabel) · \(Format.bytes(variant.size))"
        if variant.mlx { text += " · MLX" }
        return text
    }
}

/// "34% · 6.2 of 18.2 GB · 85 MB/s · 2 min left"
private func downloadText(_ progress: DownloadProgress) -> String {
    guard let fraction = progress.fraction else { return progress.status }
    var parts = ["\(Int(fraction * 100))%", "\(Format.compact(progress.completed)) of \(Format.compact(progress.total))"]
    if let speed = progress.bytesPerSecond, speed > 0 {
        parts.append("\(ByteCountFormatter.string(fromByteCount: Int64(speed), countStyle: .file))/s")
    }
    if let seconds = progress.secondsLeft {
        parts.append(seconds < 60 ? "<1 min left" : seconds < 3600 ? "\(Int(seconds / 60)) min left" : "\(Int(seconds / 3600)) h \(Int(seconds.truncatingRemainder(dividingBy: 3600) / 60)) min left")
    }
    return parts.joined(separator: " · ")
}

private struct DownloadControl: View {
    @Environment(ModelStore.self) private var store
    let pullName: String
    let variant: CatalogVariant
    let machine: MachineInfo?

    var body: some View {
        if store.isInstalled(pullName, isDefault: variant.isDefault) {
            Label("Installed", systemImage: "checkmark.circle.fill")
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(.green)
        } else if let progress = store.downloads[pullName] {
            VStack(alignment: .trailing, spacing: 4) {
                HStack(spacing: 6) {
                    if let fraction = progress.fraction {
                        ProgressView(value: fraction)
                            .frame(width: 110)
                            .animation(.linear(duration: 0.25), value: fraction)
                    } else {
                        ProgressView().controlSize(.small)
                    }
                    Button {
                        store.cancelDownload(pullName)
                    } label: {
                        Image(systemName: "xmark.circle.fill")
                            .foregroundStyle(.secondary)
                    }
                    .buttonStyle(.plain)
                    .pointerStyle(.link)
                    .help("Stop download")
                }
                Text(downloadText(progress))
                    .font(.system(size: 10))
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
        } else {
            let enoughDisk = machine?.hasDiskSpace(for: variant.size) ?? true
            Button("Download") {
                store.download(pullName)
            }
            .buttonStyle(.borderedProminent)
            .buttonBorderShape(.capsule)
            .controlSize(.small)
            .disabled(!enoughDisk || store.status == .offline)
            .pointerStyle(.link)
            .help(enoughDisk ? "ollama pull \(pullName)" : "Not enough free disk space")
        }
    }
}

// MARK: - Layout

/// Lays out children left to right and wraps onto new lines.
struct FlowLayout: Layout {
    var spacing: CGFloat = 6
    var lineSpacing: CGFloat = 6

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let rows = arrange(width: proposal.width ?? .infinity, subviews: subviews)
        let width = rows.map { $0.width }.max() ?? 0
        let height = rows.reduce(0) { $0 + $1.height } + lineSpacing * CGFloat(max(0, rows.count - 1))
        return CGSize(width: proposal.width ?? width, height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var y = bounds.minY
        for row in arrange(width: bounds.width, subviews: subviews) {
            var x = bounds.minX
            for index in row.indices {
                let size = subviews[index].sizeThatFits(.unspecified)
                subviews[index].place(at: CGPoint(x: x, y: y + (row.height - size.height) / 2), proposal: .unspecified)
                x += size.width + spacing
            }
            y += row.height + lineSpacing
        }
    }

    private struct Row { var indices: [Int] = []; var width: CGFloat = 0; var height: CGFloat = 0 }

    private func arrange(width: CGFloat, subviews: Subviews) -> [Row] {
        var rows: [Row] = [Row()]
        for index in subviews.indices {
            let size = subviews[index].sizeThatFits(.unspecified)
            if !rows[rows.count - 1].indices.isEmpty, rows[rows.count - 1].width + spacing + size.width > width {
                rows.append(Row())
            }
            var row = rows[rows.count - 1]
            row.width += (row.indices.isEmpty ? 0 : spacing) + size.width
            row.height = max(row.height, size.height)
            row.indices.append(index)
            rows[rows.count - 1] = row
        }
        return rows.filter { !$0.indices.isEmpty }
    }
}
