// Draws the app icon: swift scripts/make_icon.swift out.png
import AppKit
let size: CGFloat = 1024
let img = NSImage(size: NSSize(width: size, height: size))
img.lockFocus()
let rect = NSRect(x: 80, y: 80, width: size - 160, height: size - 160)
let bg = NSBezierPath(roundedRect: rect, xRadius: 190, yRadius: 190)
NSGradient(starting: NSColor(red: 0.20, green: 0.22, blue: 0.35, alpha: 1), ending: NSColor(red: 0.08, green: 0.09, blue: 0.14, alpha: 1))!.draw(in: bg, angle: -90)
func bubble(_ r: NSRect, _ c: NSColor, tailLeft: Bool) {
    let p = NSBezierPath(roundedRect: r, xRadius: 70, yRadius: 70)
    let tx = tailLeft ? r.minX + 90 : r.maxX - 90
    p.move(to: NSPoint(x: tx - 40, y: r.minY + 10)); p.line(to: NSPoint(x: tx + (tailLeft ? -50 : 50), y: r.minY - 70)); p.line(to: NSPoint(x: tx + 40, y: r.minY + 10)); p.close()
    c.setFill(); p.fill()
}
bubble(NSRect(x: 190, y: 470, width: 480, height: 300), NSColor(red: 0.85, green: 0.47, blue: 0.34, alpha: 1), tailLeft: true)
bubble(NSRect(x: 360, y: 230, width: 480, height: 280), NSColor(red: 0.06, green: 0.64, blue: 0.50, alpha: 1), tailLeft: false)
let dots = NSColor.white
for (i, x) in [470.0, 600.0, 730.0].enumerated() { _ = i; dots.setFill(); NSBezierPath(ovalIn: NSRect(x: x - 32, y: 338, width: 64, height: 64)).fill() }
img.unlockFocus()
let rep = NSBitmapImageRep(data: img.tiffRepresentation!)!
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
