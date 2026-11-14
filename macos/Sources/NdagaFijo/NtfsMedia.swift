/*
 * Ndaga Fijo - NTFS installation media (Rufus' NTFS + UEFI:NTFS layout)
 * Based on Rufus, Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 *
 * macOS cannot create or write NTFS, and FAT32 cannot hold files over 4 GB (e.g. a Windows install.esd).
 * Layout, as Rufus makes it:
 *   partition 1: NTFS, all of the drive but the last MiB, with the ms-sys NTFS boot record (legacy BIOS);
 *   partition 2: UEFI:NTFS, a 1 MiB FAT image whose signed loader lets UEFI firmware boot from partition 1.
 * Steps: `mkntfs` (ntfs-3g) formats a sparse image on the Mac, `ndaga-ntfs` (VOLUMA's NTFS driver) copies
 * the files into it, then only its in-use clusters are streamed to the drive by parallel writer threads. The partition table is
 * written last, so a failure part way leaves no half-made volume for macOS to mount.
 */
import Foundation

enum NtfsMedia {
    struct Failure: Error { let message: String }

    static let sector: UInt64 = 512
    static let align: UInt64 = 2048                    // 1 MiB in sectors
    static let gptReserve: UInt64 = 33                 // backup GPT entries + header

    /// Bundled tools live next to the executable (Contents/MacOS); NDAGA_TOOLS overrides for development.
    static var toolsDir: String {
        if let d = ProcessInfo.processInfo.environment["NDAGA_TOOLS"], !d.isEmpty { return d }
        return URL(fileURLWithPath: CommandLine.arguments[0]).resolvingSymlinksInPath().deletingLastPathComponent().path
    }
    /// Executables sit in Contents/MacOS, data (the UEFI:NTFS image) in Contents/Resources.
    static func tool(_ name: String) -> String {
        let here = (toolsDir as NSString).appendingPathComponent(name)
        let res = ((toolsDir as NSString).deletingLastPathComponent as NSString).appendingPathComponent("Resources/\(name)")
        return FileManager.default.fileExists(atPath: here) || !FileManager.default.fileExists(atPath: res) ? here : res
    }
    static var missingTools: [String] {
        ["mkntfs", "ndaga-ntfs", "uefi-ntfs.img"].filter { !FileManager.default.fileExists(atPath: tool($0)) }
    }

    struct Layout {
        let total: UInt64, p1Start: UInt64, p1Sectors: UInt64, p2Start: UInt64, p2Sectors: UInt64
        init(totalSectors: UInt64, uefiSectors: UInt64) {
            total = totalSectors
            p1Start = NtfsMedia.align
            p2Sectors = uefiSectors
            p2Start = (totalSectors - NtfsMedia.gptReserve - uefiSectors) / NtfsMedia.align * NtfsMedia.align
            p1Sectors = p2Start - p1Start
        }
    }

    static func write(scan: TreeCopy.Scan, source: URL, image: String? = nil, drive: Drive, gpt: Bool, label: String, rep: Reporter) throws {
        if !missingTools.isEmpty { throw Failure(message: "NTFS support files are missing from the app (\(missingTools.joined(separator: ", "))). The drive was not modified.") }
        let uefi = try Data(contentsOf: URL(fileURLWithPath: tool("uefi-ntfs.img")))
        guard uefi.count > 0, uefi.count % 512 == 0 else { throw Failure(message: "Bad UEFI:NTFS image") }

        let totalSectors = drive.size / sector
        guard totalSectors > 64 * align else { throw Failure(message: "The drive is too small for NTFS") }
        let L = Layout(totalSectors: totalSectors, uefiSectors: UInt64(uefi.count) / sector)
        let partBytes = L.p1Sectors * sector
        // NTFS metadata ($MFT, $LogFile, ...) plus a cluster of slack per file.
        let need = scan.total + UInt64(scan.files.count + scan.dirs.count) * 4096 + partBytes / 50 + (96 << 20)
        guard need < partBytes else { throw Failure(message: "The image contents do not fit on the drive as NTFS. The drive was not modified.") }

        // Stage on the system disk, or on the ISO's own volume when the system disk is short of space.
        let stageDir = Staging.disabled ? nil : Staging.directory(for: scan.total, near: image)
        let staged = stageDir != nil
        let img = ((stageDir ?? "/private/var/tmp") as NSString).appendingPathComponent(".ndagafijo-\(UUID().uuidString)-ntfs.img")
        let sfd = open(img, O_RDWR | O_CREAT | O_EXCL, 0o600)
        guard sfd >= 0 else { throw Failure(message: "Cannot create the staging image") }
        defer { unlink(img) }
        guard ftruncate(sfd, off_t(partBytes)) == 0 else { close(sfd); throw Failure(message: "Not enough space on this Mac for the staging image") }
        close(sfd)

        // 1. Format the (sparse) image. -p/-H/-S fill in the BPB fields the BIOS boot code relies on.
        rep.progress(0.06, "Formatting NTFS...")
        let vol = String(label.filter { !"/\\:*?\"<>|".contains($0) }.prefix(32))
        let mk = Shell.run(tool("mkntfs"), ["-F", "-Q", "-q", "-L", vol.isEmpty ? "NDAGAFIJO" : vol, "-c", "4096", "-s", "512",
                                             "-p", String(L.p1Start), "-H", "255", "-S", "63", img])
        guard mk.status == 0 else { throw Failure(message: "mkntfs failed: \(mk.text)") }
        try installBootRecord(img: img, bytes: partBytes)
        rep.log("Formatted NTFS (\(ByteCountFormatter.string(fromByteCount: Int64(partBytes), countStyle: .file)), 4 KB clusters)")

        let base = L.p1Start * sector
        try Helper.unmount(drive, rep)
        try zeroOldTables(drive: drive, L: L)
        if staged {
            rep.log("Using the fast path: copy into a staged NTFS image in \(stageDir!), then one sequential write with parallel writers")
            try runTool(["copy", "--target", img, "--src", source.path], rep: rep, from: 0.08, to: 0.45, label: "Copying")
            try Helper.unmount(drive, rep)
            try runTool(["stream", "--image", img, "--target", drive.rawPath, "--offset", String(base)],
                        rep: rep, from: 0.45, to: 0.98, label: "Writing to the drive")
        } else {
            rep.log(Staging.disabled ? "Staging is off (NDAGA_NO_STAGING), writing to the drive directly" : "Not enough free space on this Mac to stage the image, writing to the drive directly")
            try runTool(["stream", "--image", img, "--target", drive.rawPath, "--offset", String(base)],
                        rep: rep, from: 0.07, to: 0.08, label: "Writing NTFS metadata")
            try Helper.unmount(drive, rep)
            // ndaga-ntfs keeps the driver's small metadata writes in memory until the end and sends file data in
            // large aligned batches on parallel writers, so the raw node (much faster than /dev/diskN) works.
            try runTool(["copy", "--target", drive.rawPath, "--offset", String(base), "--length", String(partBytes),
                         "--src", source.path], rep: rep, from: 0.08, to: 0.98, label: "Copying")
        }

        rep.progress(0.985, "Writing UEFI:NTFS and the partition table...")
        try Helper.unmount(drive, rep)
        try writeTables(drive: drive, L: L, gpt: gpt, uefi: uefi)
        rep.log("Partitioned \(gpt ? "GPT" : "MBR"): NTFS + UEFI:NTFS (1 MB)\(gpt ? "" : ", Windows BIOS boot code installed")")
        BiosBoot.remount(drive.id)
    }

    // MARK: - steps

    /// Swaps mkntfs' "not bootable" stub for the ms-sys NTFS boot record (it loads BOOTMGR), keeping the BPB.
    /// The backup boot sector (the volume's last sector) gets the same first sector.
    static func installBootRecord(img: String, bytes: UInt64) throws {
        let fd = open(img, O_RDWR)
        guard fd >= 0 else { throw Failure(message: "Cannot reopen the staging image") }
        defer { close(fd) }
        var s0 = [UInt8](repeating: 0, count: 512)
        guard pread(fd, &s0, 512, 0) == 512, Array(s0[3..<11]) == Array("NTFS    ".utf8) else { throw Failure(message: "mkntfs did not make an NTFS volume") }
        guard pwrite(fd, BootCode.ntfsBr0x0, BootCode.ntfsBr0x0.count, 0) == BootCode.ntfsBr0x0.count,
              pwrite(fd, BootCode.ntfsBr0x54, BootCode.ntfsBr0x54.count, 0x54) == BootCode.ntfsBr0x54.count,
              pread(fd, &s0, 512, 0) == 512
        else { throw Failure(message: "Cannot write the NTFS boot record") }
        let totalSectors = s0[0x28..<0x30].reversed().reduce(UInt64(0)) { $0 << 8 | UInt64($1) }
        let backup = off_t(min(totalSectors * sector, bytes - sector))
        guard pwrite(fd, s0, 512, backup) == 512 else { throw Failure(message: "Cannot write the backup boot sector") }
        fsync(fd)
    }

    /// Runs a bundled `ndaga-ntfs` subcommand, turning its `P done total name` lines into progress.
    static func runTool(_ args: [String], rep: Reporter, from: Double, to: Double, label: String) throws {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: tool("ndaga-ntfs"))
        p.arguments = args
        let out = Pipe(), err = Pipe()
        p.standardOutput = out; p.standardError = err
        try p.run()
        var errText = Data()
        let errQ = DispatchQueue(label: "ndaga-ntfs-stderr")
        errQ.async { errText = err.fileHandleForReading.readDataToEndOfFile() }

        let started = Date()
        var pending = ""
        let fh = out.fileHandleForReading
        while true {
            if rep.cancelled { p.terminate(); p.waitUntilExit(); throw HelperError(message: "Cancelled by user") }
            let chunk = fh.availableData
            if chunk.isEmpty { break }
            pending += String(decoding: chunk, as: UTF8.self)
            while let nl = pending.firstIndex(of: "\n") {
                let line = String(pending[..<nl]); pending = String(pending[pending.index(after: nl)...])
                if line.hasPrefix("L ") { rep.log(String(line.dropFirst(2))); continue }
                let f = line.split(separator: " ", maxSplits: 3, omittingEmptySubsequences: false)
                guard f.count >= 3, f[0] == "P", let done = Double(f[1]), let total = Double(f[2]), total > 0 else { continue }
                let secs = max(Date().timeIntervalSince(started), 0.001)
                let rate = done / secs
                if rate > 0, done < total { rep.eta((total - done) / rate) }
                var name = f.count > 3 && f[3] != "done" ? String(f[3]) : ""
                if name.count > 44 { name = "…" + name.suffix(43) }
                rep.progress(from + (to - from) * done / total,
                             String(format: "%@%@ %.0f MB/s", label, name.isEmpty ? "..." : " " + name, rate / 1_000_000))
            }
        }
        p.waitUntilExit()
        errQ.sync {}
        if p.terminationStatus != 0 {
            let msg = String(decoding: errText, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
            throw Failure(message: msg.isEmpty ? "ndaga-ntfs \(args[0]) failed (status \(p.terminationStatus))" : msg)
        }
    }

    private static func openRaw(_ drive: Drive) throws -> Int32 {
        let fd = open(drive.rawPath, O_RDWR)
        guard fd >= 0 else { throw Failure(message: "Cannot open \(drive.rawPath): \(String(cString: strerror(errno)))") }
        return fd
    }
    private static func put(_ fd: Int32, _ bytes: [UInt8], at lba: UInt64) throws {
        guard pwrite(fd, bytes, bytes.count, off_t(lba * sector)) == bytes.count else {
            throw Failure(message: "Write error at sector \(lba): \(String(cString: strerror(errno)))")
        }
    }

    /// Clears the old partition table area (except the MBR, written last) and the old backup GPT, so macOS
    /// never sees a stale table pointing into the new data.
    static func zeroOldTables(drive: Drive, L: Layout) throws {
        let fd = try openRaw(drive); defer { close(fd) }
        try put(fd, [UInt8](repeating: 0, count: Int((align - 1) * sector)), at: 1)
        try put(fd, [UInt8](repeating: 0, count: Int(gptReserve * sector)), at: L.total - gptReserve)
        fsync(fd)
    }

    static func writeTables(drive: Drive, L: Layout, gpt: Bool, uefi: Data) throws {
        let fd = try openRaw(drive); defer { close(fd) }
        try put(fd, [UInt8](uefi), at: L.p2Start)
        var mbr = [UInt8](repeating: 0, count: 512)
        if gpt {
            let entries = gptEntries(L)
            let (primary, backup) = gptHeaders(L, entries: entries)
            try put(fd, entries, at: 2)
            try put(fd, entries, at: L.total - gptReserve)
            try put(fd, backup, at: L.total - 1)
            try put(fd, primary, at: 1)
            mbrEntry(&mbr, 0, boot: false, type: 0xEE, start: 1, count: min(L.total - 1, 0xFFFF_FFFF))
        } else {
            mbr.replaceSubrange(0..<BootCode.mbrWin7.count, with: BootCode.mbrWin7)
            var sig = UInt32.random(in: 1...UInt32.max)
            withUnsafeBytes(of: &sig) { mbr.replaceSubrange(440..<444, with: $0) }
            mbrEntry(&mbr, 0, boot: true, type: 0x07, start: L.p1Start, count: L.p1Sectors)
            mbrEntry(&mbr, 1, boot: false, type: 0xEF, start: L.p2Start, count: L.p2Sectors)
        }
        mbr[510] = 0x55; mbr[511] = 0xAA
        try put(fd, mbr, at: 0)
        fsync(fd)
        _ = ioctl(fd, 0x2000_6416)                     // DKIOCSYNCHRONIZECACHE
    }

    // MARK: - partition tables

    private static func le(_ v: UInt64, _ n: Int) -> [UInt8] { (0..<n).map { UInt8((v >> (8 * UInt64($0))) & 0xFF) } }

    private static func chs(_ lba: UInt64) -> [UInt8] {
        let (h, s) = (UInt64(255), UInt64(63))
        let c = lba / (h * s)
        if c > 1023 { return [0xFE, 0xFF, 0xFF] }
        let head = (lba / s) % h, sec = lba % s + 1
        return [UInt8(head), UInt8(sec | ((c >> 2) & 0xC0)), UInt8(c & 0xFF)]
    }

    private static func mbrEntry(_ m: inout [UInt8], _ i: Int, boot: Bool, type: UInt8, start: UInt64, count: UInt64) {
        let o = 446 + 16 * i
        m[o] = boot ? 0x80 : 0
        m.replaceSubrange(o + 1..<o + 4, with: chs(start))
        m[o + 4] = type
        m.replaceSubrange(o + 5..<o + 8, with: chs(start + count - 1))
        m.replaceSubrange(o + 8..<o + 12, with: le(start, 4))
        m.replaceSubrange(o + 12..<o + 16, with: le(min(count, 0xFFFF_FFFF), 4))
    }

    /// GUID string -> on-disk mixed-endian bytes.
    static func guid(_ s: String) -> [UInt8] {
        let h = s.replacingOccurrences(of: "-", with: "")
        let b = stride(from: 0, to: 32, by: 2).map { UInt8(h[h.index(h.startIndex, offsetBy: $0)..<h.index(h.startIndex, offsetBy: $0 + 2)], radix: 16)! }
        return [b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6]] + b[8..<16]
    }
    private static func randomGuid() -> [UInt8] { guid(UUID().uuidString) }
    static let basicData = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"

    static func gptEntries(_ L: Layout) -> [UInt8] {
        var e = [UInt8](repeating: 0, count: 128 * 128)
        func entry(_ i: Int, start: UInt64, count: UInt64, attrs: UInt64, name: String) {
            let o = i * 128
            e.replaceSubrange(o..<o + 16, with: guid(basicData))   // as Rufus: a second ESP upsets Windows Setup
            e.replaceSubrange(o + 16..<o + 32, with: randomGuid())
            e.replaceSubrange(o + 32..<o + 40, with: le(start, 8))
            e.replaceSubrange(o + 40..<o + 48, with: le(start + count - 1, 8))
            e.replaceSubrange(o + 48..<o + 56, with: le(attrs, 8))
            for (k, u) in name.utf16.prefix(36).enumerated() { e[o + 56 + 2 * k] = UInt8(u & 0xFF); e[o + 57 + 2 * k] = UInt8(u >> 8) }
        }
        entry(0, start: L.p1Start, count: L.p1Sectors, attrs: 0, name: "Main Data Partition")
        entry(1, start: L.p2Start, count: L.p2Sectors, attrs: 1 << 63, name: "UEFI:NTFS")   // no drive letter
        return e
    }

    static func gptHeaders(_ L: Layout, entries: [UInt8]) -> ([UInt8], [UInt8]) {
        let disk = randomGuid(), ecrc = crc32(entries)
        func header(current: UInt64, backup: UInt64, entriesAt: UInt64) -> [UInt8] {
            var h = [UInt8](repeating: 0, count: 512)
            h.replaceSubrange(0..<8, with: Array("EFI PART".utf8))
            h.replaceSubrange(8..<12, with: [0, 0, 1, 0])
            h.replaceSubrange(12..<16, with: le(92, 4))
            h.replaceSubrange(24..<32, with: le(current, 8))
            h.replaceSubrange(32..<40, with: le(backup, 8))
            h.replaceSubrange(40..<48, with: le(34, 8))
            h.replaceSubrange(48..<56, with: le(L.total - gptReserve - 1, 8))
            h.replaceSubrange(56..<72, with: disk)
            h.replaceSubrange(72..<80, with: le(entriesAt, 8))
            h.replaceSubrange(80..<84, with: le(128, 4))
            h.replaceSubrange(84..<88, with: le(128, 4))
            h.replaceSubrange(88..<92, with: le(UInt64(ecrc), 4))
            h.replaceSubrange(16..<20, with: le(UInt64(crc32(Array(h[0..<92]))), 4))
            return h
        }
        return (header(current: 1, backup: L.total - 1, entriesAt: 2),
                header(current: L.total - 1, backup: 1, entriesAt: L.total - gptReserve))
    }

    static func crc32(_ b: [UInt8]) -> UInt32 {
        var c: UInt32 = 0xFFFF_FFFF
        for x in b {
            c ^= UInt32(x)
            for _ in 0..<8 { c = (c >> 1) ^ (0xEDB8_8320 & (0 &- (c & 1))) }
        }
        return ~c
    }
}
