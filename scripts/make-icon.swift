// Renders Support/AppIcon.icns and docs/images/icon.png from the app's mark.
//
//   swiftc -swift-version 6 -parse-as-library -o /tmp/make-icon scripts/make-icon.swift Sources/LocalRouter/AppMark.swift && /tmp/make-icon
import AppKit

func render(_ size: CGFloat) -> Data {
    let image = NSImage(size: NSSize(width: size, height: size), flipped: false) { rect in
        // macOS app icon grid: an 824/1024 rounded square, centred.
        let inset = size * 100 / 1024
        let tile = rect.insetBy(dx: inset, dy: inset)
        let shape = NSBezierPath(roundedRect: tile, xRadius: tile.width * 0.225, yRadius: tile.width * 0.225)
        NSGradient(colors: [NSColor(srgbRed: 0.35, green: 0.45, blue: 1, alpha: 1),
                            NSColor(srgbRed: 0.16, green: 0.2, blue: 0.62, alpha: 1)])!
            .draw(in: shape, angle: -90)
        let markSize = tile.width * 0.62
        let mark = NSImage.localRouterMark(size: markSize)
        let white = NSImage(size: mark.size, flipped: false) { r in
            mark.draw(in: r)
            NSColor.white.set()
            r.fill(using: .sourceAtop)
            return true
        }
        white.draw(in: NSRect(x: tile.midX - markSize / 2, y: tile.midY - markSize / 2, width: markSize, height: markSize))
        return true
    }
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    image.draw(in: NSRect(x: 0, y: 0, width: size, height: size))
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

@main
enum MakeIcon {
    static func main() {
        let iconset = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "AppIcon.iconset")
        try? FileManager.default.removeItem(at: iconset)
        try! FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
        for base in [16, 32, 128, 256, 512] {
            try! render(CGFloat(base)).write(to: iconset.appending(path: "icon_\(base)x\(base).png"))
            try! render(CGFloat(base * 2)).write(to: iconset.appending(path: "icon_\(base)x\(base)@2x.png"))
        }
        let iconutil = Process()
        iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
        iconutil.arguments = ["-c", "icns", iconset.path, "-o", "Support/AppIcon.icns"]
        try! iconutil.run()
        iconutil.waitUntilExit()
        try! render(256).write(to: URL(fileURLWithPath: "docs/images/icon.png"))
        print("Wrote Support/AppIcon.icns and docs/images/icon.png")
    }
}
