/*
 * Rufus for macOS - process helpers
 * Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 */
import Foundation

enum Shell {
    struct Result {
        var status: Int32
        var out: Data
        var text: String { String(decoding: out, as: UTF8.self) }
    }

    /// Runs a tool and captures stdout. stderr is merged unless `mergeStderr` is false (plist output).
    @discardableResult
    static func run(_ tool: String, _ args: [String], mergeStderr: Bool = true) -> Result {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: tool)
        p.arguments = args
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = mergeStderr ? pipe : FileHandle.nullDevice
        do { try p.run() } catch { return Result(status: -1, out: Data(error.localizedDescription.utf8)) }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return Result(status: p.terminationStatus, out: data)
    }

    static func plist(_ tool: String, _ args: [String]) -> [String: Any]? {
        let r = run(tool, args, mergeStderr: false)
        guard r.status == 0,
              let obj = try? PropertyListSerialization.propertyList(from: r.out, format: nil) as? [String: Any]
        else { return nil }
        return obj
    }

    static func diskutil(_ args: [String]) -> Result { run("/usr/sbin/diskutil", args) }
    static func diskutilPlist(_ args: [String]) -> [String: Any]? { plist("/usr/sbin/diskutil", args) }

    /// Quotes a string for /bin/sh.
    static func quote(_ s: String) -> String { "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'" }
}
