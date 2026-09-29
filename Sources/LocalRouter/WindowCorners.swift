import AppKit
import SwiftUI

/// The system draws the menu-bar window with small corners. This finds the
/// window's background view and masks it with a larger radius, like the
/// system Control Center panels.
/// Sits behind the menu content, sized exactly like it. It rounds the
/// window's corners and keeps the window's height equal to the content's:
/// MenuBarExtra grows its window with the content but never shrinks it,
/// which left empty space after collapsing a section.
struct MenuWindowSizer: NSViewRepresentable {
    let radius: CGFloat

    func makeNSView(context: Context) -> SizerView {
        let view = SizerView()
        view.radius = radius
        return view
    }

    func updateNSView(_ nsView: SizerView, context: Context) {}

    final class SizerView: NSView {
        var radius: CGFloat = 18

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            guard let window else { return }
            roundCorners(of: window)
            fitWindow()
        }

        override func setFrameSize(_ newSize: NSSize) {
            super.setFrameSize(newSize)
            fitWindow()
        }

        /// Resize the window to this view's height, keeping the top edge
        /// pinned under the menu bar.
        private func fitWindow() {
            guard let window, frame.height > 0, abs(window.frame.height - frame.height) > 0.5 else { return }
            var windowFrame = window.frame
            windowFrame.origin.y += windowFrame.height - frame.height
            windowFrame.size.height = frame.height
            // Defer, so AppKit isn't resized in the middle of a layout pass.
            DispatchQueue.main.async {
                window.setFrame(windowFrame, display: true, animate: false)
                window.invalidateShadow()
            }
        }

        private func roundCorners(of window: NSWindow) {
            guard let frameView = window.contentView?.superview else { return }
            window.isOpaque = false
            window.backgroundColor = .clear
            for effect in frameView.allSubviews.compactMap({ $0 as? NSVisualEffectView }) {
                effect.maskImage = .roundedMask(radius: radius)
            }
            frameView.wantsLayer = true
            frameView.layer?.cornerRadius = radius
            frameView.layer?.cornerCurve = .continuous
            frameView.layer?.masksToBounds = true
            window.invalidateShadow()
        }
    }
}

private extension NSView {
    var allSubviews: [NSView] { [self] + subviews.flatMap(\.allSubviews) }
}

private extension NSImage {
    /// A resizable mask: only the corners are fixed, the middle stretches.
    static func roundedMask(radius: CGFloat) -> NSImage {
        let edge = radius * 2 + 1
        let image = NSImage(size: NSSize(width: edge, height: edge), flipped: false) { rect in
            NSColor.black.setFill()
            NSBezierPath(roundedRect: rect, xRadius: radius, yRadius: radius).fill()
            return true
        }
        image.capInsets = NSEdgeInsets(top: radius, left: radius, bottom: radius, right: radius)
        image.resizingMode = .stretch
        return image
    }
}

/// Brings a window forward when it opens. A menu-bar app isn't active by
/// default, so SwiftUI windows can open behind other apps.
struct BringToFront: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async {
            NSApp.activate()
            view.window?.makeKeyAndOrderFront(nil)
            view.window?.orderFrontRegardless()
        }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {}
}

