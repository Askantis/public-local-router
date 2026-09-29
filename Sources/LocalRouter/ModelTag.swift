import SwiftUI

/// What a model is used for. Stored by raw value, so the order and labels can change.
/// Cases are alphabetical, which is the order menus and chips show them in.
enum ModelTag: String, CaseIterable, Identifiable {
    case coding, researching, talking, working, writing

    var id: String { rawValue }
    var label: String { rawValue.capitalized }

    var systemImage: String {
        switch self {
        case .coding: "chevron.left.forwardslash.chevron.right"
        case .writing: "pencil"
        case .working: "briefcase"
        case .talking: "bubble.left.and.bubble.right"
        case .researching: "magnifyingglass"
        }
    }

    var color: Color {
        switch self {
        case .coding: .blue
        case .writing: .orange
        case .working: .indigo
        case .talking: .green
        case .researching: .purple
        }
    }
}
