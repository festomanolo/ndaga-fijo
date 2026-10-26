/*
 * Rufus for macOS - boot image inspection and checksums
 * Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 */
import Foundation
import CryptoKit

struct ImageInfo {
    var size: UInt64
    var isOptical: Bool     // ISO9660 / UDF volume descriptor present
    var isHybrid: Bool      // carries an MBR boot signature, so it can be written as-is

    /// Hybrid images and raw disk images are written block for block; plain ISOs are extracted.
    var recommendedMode: WriteMode { (isOptical && !isHybrid) ? .extract : .dd }

    var summary: String {
        let sz = ByteCountFormatter.string(fromByteCount: Int64(size), countStyle: .file)
        if isOptical && isHybrid { return "Hybrid ISO, \(sz) - can be written directly (DD mode)" }
        if isOptical { return "ISO image, \(sz) - files will be extracted to the drive" }
        return "Disk image, \(sz) - will be written directly (DD mode)"
    }

    static func inspect(_ path: String) -> ImageInfo? {
        guard let fh = FileHandle(forReadingAtPath: path) else { return nil }
        defer { try? fh.close() }
        let size = (try? FileManager.default.attributesOfItem(atPath: path)[.size] as? UInt64) ?? 0
        guard size > 0 else { return nil }

        let head = (try? fh.read(upToCount: 512)) ?? Data()
        var hybrid = false
        if head.count == 512, head[510] == 0x55, head[511] == 0xAA {
            hybrid = head.prefix(510).contains { $0 != 0 }
        }
        var optical = false
        if size > 0x8006, (try? fh.seek(toOffset: 0x8001)) != nil, let id = try? fh.read(upToCount: 5) {
            let s = String(decoding: id, as: UTF8.self)
            optical = (s == "CD001" || s == "BEA01")
        }
        return ImageInfo(size: size, isOptical: optical, isHybrid: hybrid)
    }
}

enum Checksums {
    struct Result { var md5, sha1, sha256, sha512: String }

    static func compute(path: String, progress: (Double) -> Void) -> Result? {
        guard let fh = FileHandle(forReadingAtPath: path) else { return nil }
        defer { try? fh.close() }
        let total = Double((try? FileManager.default.attributesOfItem(atPath: path)[.size] as? UInt64) ?? 1)
        var md5 = Insecure.MD5(), sha1 = Insecure.SHA1(), sha256 = SHA256(), sha512 = SHA512()
        var done = 0.0
        while true {
            let chunk = autoreleasepool { try? fh.read(upToCount: 4 << 20) } ?? Data()
            if chunk.isEmpty { break }
            md5.update(data: chunk); sha1.update(data: chunk)
            sha256.update(data: chunk); sha512.update(data: chunk)
            done += Double(chunk.count)
            progress(done / total)
        }
        func hex<D: Digest>(_ d: D) -> String { d.map { String(format: "%02x", $0) }.joined() }
        return Result(md5: hex(md5.finalize()), sha1: hex(sha1.finalize()),
                      sha256: hex(sha256.finalize()), sha512: hex(sha512.finalize()))
    }
}
