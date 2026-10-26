// Renders the Ndaga Fijo app icon (a USB trident symbol on a rounded gradient tile).
// Usage: swift assets/make-logo.swift <output-iconset-dir>
import AppKit

func render(_ px: Int) -> Data {
    let s = CGFloat(px) / 1024
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let ctx = NSGraphicsContext.current!.cgContext
    ctx.scaleBy(x: s, y: s)

    // macOS icon tile: 824pt rounded square centred in the 1024 canvas.
    let tile = CGRect(x: 100, y: 100, width: 824, height: 824)
    let path = CGPath(roundedRect: tile, cornerWidth: 185, cornerHeight: 185, transform: nil)
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -14), blur: 28, color: NSColor.black.withAlphaComponent(0.35).cgColor)
    ctx.addPath(path); ctx.setFillColor(NSColor(red: 0.05, green: 0.2, blue: 0.45, alpha: 1).cgColor); ctx.fillPath()
    ctx.restoreGState()
    ctx.saveGState()
    ctx.addPath(path); ctx.clip()
    let grad = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: [
        NSColor(red: 0.10, green: 0.62, blue: 0.62, alpha: 1).cgColor,    // teal
        NSColor(red: 0.12, green: 0.25, blue: 0.62, alpha: 1).cgColor     // deep blue
    ] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(grad, start: CGPoint(x: 100, y: 924), end: CGPoint(x: 924, y: 100), options: [])
    // soft top highlight
    let hi = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: [
        NSColor.white.withAlphaComponent(0.22).cgColor, NSColor.white.withAlphaComponent(0).cgColor] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(hi, start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 560), options: [])
    ctx.restoreGState()

    // USB trident, white.
    ctx.setFillColor(NSColor.white.cgColor); ctx.setStrokeColor(NSColor.white.cgColor)
    ctx.setLineWidth(40); ctx.setLineCap(.round); ctx.setLineJoin(.round)
    let cx: CGFloat = 512
    let lines = CGMutablePath()
    lines.move(to: CGPoint(x: cx, y: 270)); lines.addLine(to: CGPoint(x: cx, y: 690))          // stem
    lines.move(to: CGPoint(x: cx, y: 400)); lines.addLine(to: CGPoint(x: 350, y: 400))         // left branch
    lines.addLine(to: CGPoint(x: 350, y: 520))
    lines.move(to: CGPoint(x: cx, y: 500)); lines.addLine(to: CGPoint(x: 674, y: 500))         // right branch
    lines.addLine(to: CGPoint(x: 674, y: 590))
    ctx.addPath(lines); ctx.strokePath()
    ctx.fillEllipse(in: CGRect(x: cx - 62, y: 208, width: 124, height: 124))                    // base circle
    ctx.fillEllipse(in: CGRect(x: 350 - 50, y: 520, width: 100, height: 100))                   // left end: circle
    ctx.fill(CGRect(x: 674 - 48, y: 585, width: 96, height: 96))                                // right end: square
    let tri = CGMutablePath()                                                                   // top: arrowhead
    tri.move(to: CGPoint(x: cx, y: 830)); tri.addLine(to: CGPoint(x: cx - 82, y: 690)); tri.addLine(to: CGPoint(x: cx + 82, y: 690))
    tri.closeSubpath(); ctx.addPath(tri); ctx.fillPath()

    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.iconset"
try FileManager.default.createDirectory(atPath: out, withIntermediateDirectories: true)
for (name, px) in [("icon_16x16", 16), ("icon_16x16@2x", 32), ("icon_32x32", 32), ("icon_32x32@2x", 64),
                   ("icon_128x128", 128), ("icon_128x128@2x", 256), ("icon_256x256", 256), ("icon_256x256@2x", 512),
                   ("icon_512x512", 512), ("icon_512x512@2x", 1024)] {
    try render(px).write(to: URL(fileURLWithPath: "\(out)/\(name).png"))
}
