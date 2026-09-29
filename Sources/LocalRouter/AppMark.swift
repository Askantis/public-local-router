import AppKit

extension NSImage {
    /// Local Router's mark: routes branching from one point inside a rounded
    /// box, i.e. routing that stays on this Mac. Drawn as a template image so
    /// the menu bar tints it for light and dark mode.
    ///
    /// With `badge`, a coloured dot sits in the top-right corner. The menu bar
    /// shows only one image, so the dot has to be part of it; the image is then
    /// not a template, and draws the mark in the label colour of whatever
    /// appearance it's drawn in, which matches the menu bar in light and dark.
    static func localRouterMark(size: CGFloat = 18, badge: NSColor? = nil) -> NSImage {
        let image = NSImage(size: NSSize(width: size, height: size), flipped: false) { rect in
            let s = rect.width / 18
            let ink = badge == nil ? NSColor.black : NSColor.labelColor.withAlphaComponent(1)
            ink.setStroke()
            ink.setFill()

            // The box: this Mac.
            let box = NSBezierPath(roundedRect: NSRect(x: 1.5 * s, y: 1.5 * s, width: 15 * s, height: 15 * s),
                                   xRadius: 4.2 * s, yRadius: 4.2 * s)
            box.lineWidth = 1.4 * s
            box.stroke()

            // The source node.
            let origin = NSPoint(x: 5.2 * s, y: 9 * s)
            NSBezierPath(ovalIn: NSRect(x: origin.x - 1.6 * s, y: origin.y - 1.6 * s, width: 3.2 * s, height: 3.2 * s)).fill()

            // Three routes fanning out to the right, each ending in a small dot.
            let ends = [NSPoint(x: 13 * s, y: 13 * s), NSPoint(x: 13.4 * s, y: 9 * s), NSPoint(x: 13 * s, y: 5 * s)]
            for end in ends {
                let path = NSBezierPath()
                path.move(to: origin)
                path.curve(to: end,
                           controlPoint1: NSPoint(x: 8.5 * s, y: origin.y),
                           controlPoint2: NSPoint(x: 9.5 * s, y: end.y))
                path.lineWidth = 1.3 * s
                path.lineCapStyle = .round
                path.stroke()
                NSBezierPath(ovalIn: NSRect(x: end.x - 1.1 * s, y: end.y - 1.1 * s, width: 2.2 * s, height: 2.2 * s)).fill()
            }

            if let badge {
                // Punch a ring out of the mark, then draw the dot inside it.
                let center = NSPoint(x: 14.6 * s, y: 14.6 * s)
                let ring = 4.2 * s, dot = 3.0 * s
                NSGraphicsContext.current?.compositingOperation = .clear
                NSBezierPath(ovalIn: NSRect(x: center.x - ring, y: center.y - ring, width: ring * 2, height: ring * 2)).fill()
                NSGraphicsContext.current?.compositingOperation = .sourceOver
                badge.setFill()
                NSBezierPath(ovalIn: NSRect(x: center.x - dot, y: center.y - dot, width: dot * 2, height: dot * 2)).fill()
            }
            return true
        }
        image.isTemplate = badge == nil
        return image
    }
}
