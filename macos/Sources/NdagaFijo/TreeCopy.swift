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
            next += 1
            return items[next - 1]
        }
        func fail(_ e: Error) { lock.lock(); if firstError == nil { firstError = e }; lock.unlock() }

        DispatchQueue.concurrentPerform(iterations: max(1, threads)) { _ in
            let chunk = 1 << 20
            let buf = UnsafeMutableRawPointer.allocate(byteCount: chunk, alignment: 4096)
            defer { buf.deallocate() }
            while let it = claim() {
                do { try copyOne(it, dest: dest, splitLargeWim: splitLargeWim, buf: buf, chunk: chunk, rep: rep, advance: advance) }
                catch { fail(error) }
            }
        }
        if let e = firstError { throw e }
        progress(copied, "")
    }

    private static func copyOne(_ it: Item, dest: String, splitLargeWim: Bool, buf: UnsafeMutableRawPointer, chunk: Int,
                                rep: Reporter, advance: @escaping (Int, String) -> Void) throws {
        try rep.checkCancel()
        let target = (dest as NSString).appendingPathComponent(it.rel)

        if splitLargeWim && it.size > fat32Limit && it.rel.lowercased() == "sources/install.wim" {
            rep.log("Splitting install.wim (\(ByteCountFormatter.string(fromByteCount: Int64(it.size), countStyle: .file))) to fit FAT32")
            let parts = try WimSplit.split(source: it.path, destDir: (target as NSString).deletingLastPathComponent,
                                           maxPart: 4000 << 20, onBytes: { advance($0, "install.swm") },
                                           cancelled: { rep.cancelled })
            rep.log("Wrote \(parts.count) parts")
            return
        }

        let inFD = open(it.path, O_RDONLY)
        guard inFD >= 0 else { throw Helper.errnoMessage("Cannot read \(it.rel)") }
        defer { close(inFD) }
        let outFD = open(target, O_WRONLY | O_CREAT | O_TRUNC, 0o644)
        guard outFD >= 0 else { throw Helper.errnoMessage("Cannot create \(it.rel)") }
        defer { close(outFD) }

        if it.size >= 16 << 20 {                                       // large file: overlap read and write
            _ = fcntl(outFD, F_NOCACHE, 1)
            try FastIO.copy(from: inFD, srcOffset: 0, to: outFD, length: it.size,
                            onBytes: { advance($0, it.rel) }, cancelled: { rep.cancelled })
            return
        }
        while true {
            let n = read(inFD, buf, chunk)
            if n < 0 { throw Helper.errnoMessage("Read error in \(it.rel)") }
            if n == 0 { break }
            var off = 0
            while off < n {
                let w = write(outFD, buf + off, n - off)
                if w < 0 { throw Helper.errnoMessage("Write error in \(it.rel)") }
                off += w
            }
            advance(n, it.rel)
        }
    }
}
