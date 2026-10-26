/*
 * Rufus for macOS - WIM splitter
 * Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 *
 * Splits a Windows install.wim into install.swm, install2.swm, ... so each part fits
 * under the 4 GB FAT32 file limit. Compressed resources are copied byte for byte (no
 * recompression), only the headers and lookup tables are rewritten.
 * Layout reference: Microsoft "Windows Imaging File Format" (WIM) specification.
 */
import Foundation

enum WimSplit {
    struct Failure: Error { let message: String }

    private static let headerSize = 208
    private static let entrySize = 50
    private static let flagMetadata: UInt8 = 0x02
    private static let flagSpanned: UInt8 = 0x08
    private static let flagSolid: UInt8 = 0x10
    private static let hdrFlagSpanned: UInt32 = 0x08

    // MARK: little-endian helpers

    private static func le16(_ b: [UInt8], _ o: Int) -> UInt16 { UInt16(b[o]) | UInt16(b[o + 1]) << 8 }
    private static func le32(_ b: [UInt8], _ o: Int) -> UInt32 { UInt32(le16(b, o)) | UInt32(le16(b, o + 2)) << 16 }
    private static func le64(_ b: [UInt8], _ o: Int) -> UInt64 { UInt64(le32(b, o)) | UInt64(le32(b, o + 4)) << 32 }
    private static func put(_ b: inout [UInt8], _ o: Int, _ v: UInt64, _ n: Int) {
        for i in 0..<n { b[o + i] = UInt8((v >> (8 * UInt64(i))) & 0xFF) }
    }

    /// 24-byte resource header: 7-byte size, 1-byte flags, offset, original size.
    private struct Res {
        var size: UInt64 = 0, flags: UInt8 = 0, offset: UInt64 = 0, original: UInt64 = 0
        init() {}
        init(size: UInt64, flags: UInt8, offset: UInt64, original: UInt64) {
            self.size = size; self.flags = flags; self.offset = offset; self.original = original
        }
        init(_ b: [UInt8], _ o: Int) {
            var s: UInt64 = 0
            for i in 0..<7 { s |= UInt64(b[o + i]) << (8 * UInt64(i)) }
            size = s; flags = b[o + 7]; offset = WimSplit.le64(b, o + 8); original = WimSplit.le64(b, o + 16)
        }
        func encode(into b: inout [UInt8], at o: Int) {
            WimSplit.put(&b, o, size, 7); b[o + 7] = flags
            WimSplit.put(&b, o + 8, offset, 8); WimSplit.put(&b, o + 16, original, 8)
        }
    }

    private struct Entry {
        var res: Res
        var refcount: UInt32
        var hash: [UInt8]
        var isMetadata: Bool { res.flags & WimSplit.flagMetadata != 0 }
    }

    private static func readExact(_ fd: Int32, _ count: Int, at offset: UInt64) throws -> [UInt8] {
        var out = [UInt8](repeating: 0, count: count)
        var got = 0
        while got < count {
            let n = out.withUnsafeMutableBytes { pread(fd, $0.baseAddress! + got, count - got, off_t(offset) + off_t(got)) }
            if n < 0 { if errno == EINTR { continue }; throw Failure(message: "Read error: \(String(cString: strerror(errno)))") }
            if n == 0 { throw Failure(message: "The WIM file is truncated") }
            got += n
        }
        return out
    }

    private static func writeAll(_ fd: Int32, _ bytes: [UInt8]) throws {
        var off = 0
        while off < bytes.count {
            let n = bytes.withUnsafeBytes { write(fd, $0.baseAddress! + off, bytes.count - off) }
            if n < 0 { if errno == EINTR { continue }; throw Failure(message: "Write error: \(String(cString: strerror(errno)))") }
            off += n
        }
    }

    // MARK: split

    /// Returns the paths of the parts written. `maxPart` is the largest allowed part size in bytes.
    static func split(source: String, destDir: String, base: String = "install", maxPart: UInt64,
                      onBytes: @escaping (Int) -> Void, cancelled: @escaping () -> Bool) throws -> [String] {
        let src = open(source, O_RDONLY)
        guard src >= 0 else { throw Failure(message: "Cannot open \(source): \(String(cString: strerror(errno)))") }
        defer { close(src) }

        var hdr = try readExact(src, headerSize, at: 0)
        guard Array(hdr[0..<5]) == Array("MSWIM".utf8), le32(hdr, 8) == UInt32(headerSize) else {
            throw Failure(message: "install.wim is not a valid WIM file")
        }
        guard le32(hdr, 12) == 0x10D00 else {
            throw Failure(message: "install.wim uses a solid/ESD layout that cannot be split")
        }
        guard le16(hdr, 42) <= 1, le32(hdr, 16) & hdrFlagSpanned == 0 else {
            throw Failure(message: "install.wim is already part of a split set")
        }

        let tableRes = Res(hdr, 48), xmlRes = Res(hdr, 72), bootRes = Res(hdr, 96)
        guard tableRes.size % UInt64(entrySize) == 0, tableRes.size < (1 << 31) else {
            throw Failure(message: "Corrupt lookup table")
        }
        let tableBytes = try readExact(src, Int(tableRes.size), at: tableRes.offset)
        let xml = try readExact(src, Int(xmlRes.size), at: xmlRes.offset)

        var entries: [Entry] = []
        for i in 0..<(tableBytes.count / entrySize) {
            let o = i * entrySize
            var e = Entry(res: Res(tableBytes, o), refcount: le32(tableBytes, o + 26), hash: Array(tableBytes[(o + 30)..<(o + 50)]))
            if e.res.flags & (flagSolid | flagSpanned) != 0 { throw Failure(message: "install.wim uses a solid/ESD layout that cannot be split") }
            if e.res.size == 0 && e.res.original == 0 { e.res.offset = 0 }
            entries.append(e)
        }
        entries.sort { $0.res.offset < $1.res.offset }

        // Plan: pack entries in file order; metadata always lands in part 1.
        let fixed = UInt64(headerSize) + UInt64(xml.count)
        var parts: [[Entry]] = [[]]
        var used: [UInt64] = [fixed]
        let metadata = entries.filter { $0.isMetadata }
        for m in metadata { parts[0].append(m); used[0] += m.res.size + UInt64(entrySize) }
        for e in entries where !e.isMetadata {
            let cost = e.res.size + UInt64(entrySize)
            if fixed + cost > maxPart { throw Failure(message: "A single file inside install.wim is larger than the part limit") }
            if used[parts.count - 1] + cost > maxPart { parts.append([]); used.append(fixed) }
            parts[parts.count - 1].append(e); used[parts.count - 1] += cost
        }
        guard used[0] <= maxPart else { throw Failure(message: "The WIM metadata is larger than the part limit") }

        // Write.
        let guid = Array(hdr[24..<40])
        _ = guid
        var written: [String] = []
        for (idx, part) in parts.enumerated() {
            let name = idx == 0 ? "\(base).swm" : "\(base)\(idx + 1).swm"
            let path = (destDir as NSString).appendingPathComponent(name)
            let out = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0o644)
            guard out >= 0 else { throw Failure(message: "Cannot create \(name): \(String(cString: strerror(errno)))") }
            defer { close(out) }
            _ = fcntl(out, F_NOCACHE, 1)
            written.append(path)

            try writeAll(out, [UInt8](repeating: 0, count: headerSize))
            var pos = UInt64(headerSize)
            var newEntries: [Entry] = []
            var newBoot = Res()

            // Entries sorted by source offset; contiguous ones are copied as one range.
            var i = 0
            let ordered = part.sorted { $0.res.offset < $1.res.offset }
            while i < ordered.count {
                var j = i
                var runEnd = ordered[i].res.offset + ordered[i].res.size
                while j + 1 < ordered.count, ordered[j + 1].res.offset == runEnd {
                    j += 1; runEnd += ordered[j].res.size
                }
                let runStart = ordered[i].res.offset
                if runEnd > runStart {
                    try FastIO.copy(from: src, srcOffset: Int64(runStart), to: out, length: runEnd - runStart,
                                    onBytes: onBytes, cancelled: cancelled)
                }
                for k in i...j {
                    var e = ordered[k]
                    let old = e.res.offset
                    e.res.offset = pos + (old - runStart)
                    if e.isMetadata, old == bootRes.offset, bootRes.size > 0, idx == 0 { newBoot = e.res }
                    newEntries.append(e)
                }
                pos += runEnd - runStart
                i = j + 1
            }

            var table = [UInt8](repeating: 0, count: newEntries.count * entrySize)
            for (n, e) in newEntries.enumerated() {
                let o = n * entrySize
                e.res.encode(into: &table, at: o)
                put(&table, o + 24, UInt64(idx + 1), 2)
                put(&table, o + 26, UInt64(e.refcount), 4)
                table.replaceSubrange((o + 30)..<(o + 50), with: e.hash)
            }
            let tableOffset = pos
            try writeAll(out, table); pos += UInt64(table.count)
            let xmlOffset = pos
            try writeAll(out, xml); pos += UInt64(xml.count)

            var h = hdr
            put(&h, 16, UInt64(le32(hdr, 16) | hdrFlagSpanned), 4)
            put(&h, 40, UInt64(idx + 1), 2)
            put(&h, 42, UInt64(parts.count), 2)
            Res(size: UInt64(table.count), flags: 0, offset: tableOffset, original: UInt64(table.count)).encode(into: &h, at: 48)
            Res(size: UInt64(xml.count), flags: 0, offset: xmlOffset, original: UInt64(xml.count)).encode(into: &h, at: 72)
            newBoot.encode(into: &h, at: 96)
            put(&h, 120, idx == 0 ? UInt64(le32(hdr, 120)) : 0, 4)
            Res().encode(into: &h, at: 124)                       // no integrity table
            guard pwrite(out, h, headerSize, 0) == headerSize else {
                throw Failure(message: "Write error: \(String(cString: strerror(errno)))")
            }
            fsync(out)
        }
        hdr = []
        return written
    }
}
