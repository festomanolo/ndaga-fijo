/*
 * Ndaga Fijo - parallel tree copier
 * Based on Rufus, Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 *
 * Copies a mounted image's files into a destination folder using several worker threads.
 * Large files go through the pipelined FastIO engine; an oversized install.wim is split on the fly.
 */
import Foundation

enum TreeCopy {
    struct Item { let path: String; let rel: String; let size: UInt64 }
    struct Scan { var dirs: [String] = []; var files: [Item] = []; var total: UInt64 = 0; var hasBootmgr = false }

    static let fat32Limit: UInt64 = 0xFFFF_FFFF

    static func scan(_ root: URL, rep: Reporter) throws -> Scan {
        let keys: [URLResourceKey] = [.isDirectoryKey, .fileSizeKey, .isRegularFileKey]
        guard let walk = FileManager.default.enumerator(at: root, includingPropertiesForKeys: keys, options: []) else {
            throw HelperError(message: "Cannot read image contents")
        }
        var s = Scan()
        for case let u as URL in walk {
            let rel = String(u.path.dropFirst(root.path.count + 1))
            let v = try? u.resourceValues(forKeys: Set(keys))
            if v?.isDirectory == true { s.dirs.append(rel); continue }
            guard v?.isRegularFile == true else { continue }          // FAT/exFAT have no symlinks
            let size = UInt64(v?.fileSize ?? 0)
            s.files.append(Item(path: u.path, rel: rel, size: size))
            s.total += size
            if rel.lowercased() == "bootmgr" { s.hasBootmgr = true }
            try rep.checkCancel()
        }
        return s
    }

    /// `progress` receives total bytes copied so far and the file being worked on, at most ~10 times a second.
    static func copy(_ s: Scan, dest: String, splitLargeWim: Bool, threads: Int, rep: Reporter,
                     progress: @escaping (UInt64, String) -> Void) throws {
        let fm = FileManager.default
        for d in s.dirs { try fm.createDirectory(atPath: (dest as NSString).appendingPathComponent(d), withIntermediateDirectories: true) }

        let items = s.files.sorted { $0.size > $1.size }          // biggest first keeps the workers balanced
        let lock = NSLock()
        var next = 0
        var firstError: Error?
        var copied: UInt64 = 0
        var lastReport = Date.distantPast

        func advance(_ n: Int, _ rel: String) {
            lock.lock(); defer { lock.unlock() }
            copied += UInt64(n)
            let now = Date()
            if now.timeIntervalSince(lastReport) > 0.1 { lastReport = now; progress(copied, rel) }
        }
        func claim() -> Item? {
            lock.lock(); defer { lock.unlock() }
            guard firstError == nil, next < items.count else { return nil }
