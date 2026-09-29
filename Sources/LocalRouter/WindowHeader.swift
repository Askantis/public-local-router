import SwiftUI

/// Every window's title bar: app icon, "Local Router", then the page name.
struct WindowHeader: View {
    let page: String
    var detail: String?

    var body: some View {
        HStack(spacing: 7) {
            AppBadge(size: 20)
            Text("Local Router")
                .font(.system(size: 13, weight: .semibold))
            Text(page)
                .font(.system(size: 13))
                .foregroundStyle(.secondary)
            if let detail {
                Text(detail)
                    .font(.system(size: 11))
                    .foregroundStyle(.tertiary)
            }
        }
        .fixedSize()
    }
}

extension View {
    /// Replaces the window's plain title with the Local Router header.
    func windowHeader(_ page: String, detail: String? = nil) -> some View {
        self
            .navigationTitle("Local Router – \(page)")
            .toolbar(removing: .title)
            .toolbar {
                ToolbarItem(placement: .navigation) {
                    WindowHeader(page: page, detail: detail)
                }
            }
    }
}
