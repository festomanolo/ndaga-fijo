/*
 * Ndaga Fijo - staged FAT32 writer
 * Based on Rufus, Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 *
 * Copying thousands of small files straight onto a USB stick through the macOS FAT driver is
 * dominated by per-file metadata round trips to the (slow) device: 34 minutes for 7,000 files
 * that a raw write moves in about one minute. Instead:
 *   1. diskutil formats the stick, and its FAT32 geometry is cloned into a sparse image on the Mac's SSD;
 *   2. the files are copied into that image in parallel, where metadata updates are cheap;
 *   3. the used part of the finished image is streamed to the stick in one sequential pass.
 */
import Foundation

enum Staging {
    struct Failure: Error { let message: String }
    static var disabled: Bool { ProcessInfo.processInfo.environment["NDAGA_NO_STAGING"] == "1" }

    private static func le16(_ b: [UInt8], _ o: Int) -> Int { Int(b[o]) | Int(b[o + 1]) << 8 }
    private static func le32(_ b: [UInt8], _ o: Int) -> Int { le16(b, o) | le16(b, o + 2) << 16 }

    /// Space needed on the Mac for the staging image: file contents plus FAT overhead.
    static func hasRoom(for total: UInt64) -> Bool {
        var st = statfs()
        guard statfs("/private/var/tmp", &st) == 0 else { return false }
        let avail = UInt64(st.f_bavail) * UInt64(st.f_bsize)
        return avail > total + total / 8 + (512 << 20)
    }

    /// Returns false (without having touched the stick's contents) if the staged route can't be used.
    static func writeFAT32(scan: TreeCopy.Scan, drive: Drive, partition: String, label: String,
                           threads: Int, rep: Reporter) throws {
        let dev = "/dev/r\(partition)"
        let pfd = open(dev, O_RDONLY)
        guard pfd >= 0 else { throw Failure(message: "Cannot open \(dev): \(String(cString: strerror(errno)))") }
        var bpb = [UInt8](repeating: 0, count: 512)
        guard pread(pfd, &bpb, 512, 0) == 512, Array(bpb[0x52..<0x5A]) == Array("FAT32   ".utf8) else {
            close(pfd); throw Failure(message: "The new volume is not FAT32")
        }
        let bps = le16(bpb, 0x0B), spc = Int(bpb[0x0D]), reserved = le16(bpb, 0x0E)
        let fats = Int(bpb[0x10]), totalSectors = le32(bpb, 0x20), fatSectors = le32(bpb, 0x24)
        guard bps == 512, spc > 0, fats > 0, fatSectors > 0, totalSectors > 0 else {
            close(pfd); throw Failure(message: "Unexpected FAT32 geometry")
        }
        let fatStart = reserved * bps
        let dataStart = (reserved + fats * fatSectors) * bps
        let clusterBytes = spc * bps
        let headBytes = dataStart + clusterBytes                       // boot area, both FATs, root directory

        let stagePath = "/private/var/tmp/ndagafijo-\(UUID().uuidString).img"
        let sfd = open(stagePath, O_RDWR | O_CREAT | O_EXCL, 0o600)
        guard sfd >= 0 else { close(pfd); throw Failure(message: "Cannot create the staging image") }
        var mount: String?
        defer {
            close(sfd)
            if let mount { _ = Shell.run("/usr/bin/hdiutil", ["detach", "-force", mount]) }
            unlink(stagePath)
        }
        guard ftruncate(sfd, off_t(totalSectors * bps)) == 0 else { close(pfd); throw Failure(message: "Not enough space for staging") }

        // 1. Clone the freshly formatted geometry.
        var head = [UInt8](repeating: 0, count: headBytes)
        let got = pread(pfd, &head, headBytes, 0)
        close(pfd)
        guard got == headBytes, pwrite(sfd, head, headBytes, 0) == headBytes else { throw Failure(message: "Cannot clone the FAT layout") }

        // 2. Attach and copy in parallel.
        rep.progress(0.08, "Preparing files...")
        let att = Shell.plist("/usr/bin/hdiutil", ["attach", "-imagekey", "diskimage-class=CRawDiskImage",
                                                     "-nobrowse", "-noverify", "-plist", stagePath])
        guard let ents = att?["system-entities"] as? [[String: Any]] else { throw Failure(message: "Cannot attach the staging image") }
        var mp = ents.compactMap { $0["mount-point"] as? String }.first
        let devNode = ents.compactMap { $0["dev-entry"] as? String }.first
        if mp == nil, let devNode {
            _ = Shell.diskutil(["mount", devNode])
            mp = Shell.diskutilPlist(["info", "-plist", devNode])?["MountPoint"] as? String
        }
        guard let mountPoint = mp else { throw Failure(message: "Cannot mount the staging image") }
        mount = mountPoint
        FileManager.default.createFile(atPath: mountPoint + "/.metadata_never_index", contents: Data())

        let started = Date()
        try TreeCopy.copy(scan, dest: mountPoint, splitLargeWim: true, threads: threads, rep: rep,
                          progress: { done, name in
            rep.progress(0.08 + 0.27 * Double(done) / Double(max(scan.total, 1)), "Preparing \(name)")
        })
        rep.log(String(format: "Prepared %@ in %.0fs", ByteCountFormatter.string(fromByteCount: Int64(scan.total), countStyle: .file),
                       Date().timeIntervalSince(started)))
        for junk in [".fseventsd", ".Spotlight-V100", ".Trashes", ".TemporaryItems", ".metadata_never_index"] {
            try? FileManager.default.removeItem(atPath: mountPoint + "/" + junk)
        }
        _ = Shell.run("/bin/sync", [])
        var detached = false
        for _ in 0..<5 {
            if Shell.run("/usr/bin/hdiutil", ["detach", mountPoint]).status == 0 { detached = true; break }
            Thread.sleep(forTimeInterval: 1)
        }
        if !detached { _ = Shell.run("/usr/bin/hdiutil", ["detach", "-force", mountPoint]) }
        mount = nil
        try rep.checkCancel()

        // 3. Find how much of the image is actually used.
        let fatBytes = fatSectors * bps
        var lastCluster = 1
        var offset = fatBytes
        var chunk = [UInt8](repeating: 0, count: 1 << 20)
        scan: while offset > 0 {
            let len = min(chunk.count, offset)
            offset -= len
            guard pread(sfd, &chunk, len, off_t(fatStart + offset)) == len else { throw Failure(message: "Cannot read the FAT") }
            var i = len - 4
            while i >= 0 {
                if (chunk[i] | chunk[i + 1] | chunk[i + 2] | chunk[i + 3 ]) != 0 { lastCluster = (offset + i) / 4; break scan }
                i -= 4
            }
        }
        let usedEnd = lastCluster >= 2 ? dataStart + (lastCluster - 1) * clusterBytes : headBytes
        let streamBytes = UInt64(min(usedEnd, totalSectors * bps))

        // 4. One sequential pass to the stick.
        try rep.checkCancel()
        let out = open(dev, O_WRONLY)
        guard out >= 0 else { throw Failure(message: "Cannot open \(dev) for writing: \(String(cString: strerror(errno)))") }
        defer { close(out) }
        _ = fcntl(out, F_NOCACHE, 1)
        var written: UInt64 = 0
        let t0 = Date()
        rep.progress(0.35, "Writing to the drive...")
        try FastIO.copy(from: sfd, srcOffset: 0, to: out, length: streamBytes, bufferSize: 8 << 20, depth: 4, padTo: 512,
                        onBytes: { n in
            written += UInt64(n)
            let secs = max(Date().timeIntervalSince(t0), 0.001)
            let rate = Double(written) / secs
            rep.progress(0.35 + 0.64 * Double(written) / Double(streamBytes),
                         String(format: "Writing to the drive... %.0f MB/s", rate / 1_000_000))
            if secs > 3 { rep.eta(Double(streamBytes - written) / rate) }
        }, cancelled: { rep.cancelled })
        fsync(out)
        rep.log(String(format: "Streamed %@ to the drive in %.0fs", ByteCountFormatter.string(fromByteCount: Int64(streamBytes), countStyle: .file),
                       Date().timeIntervalSince(t0)))
    }
}
