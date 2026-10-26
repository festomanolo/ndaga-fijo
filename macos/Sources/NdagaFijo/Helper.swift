/*
 * Ndaga Fijo - privileged write helper
 * Based on Rufus, Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 *
 * The app re-launches its own binary as root (via the standard macOS authorization prompt)
 * with `--helper`. The helper re-validates the target, does the destructive work, and reports
 * back through a progress file. Only the helper ever runs as root.
 */
import Foundation

struct HelperError: Error { let message: String }

final class Reporter {
    private let fh: FileHandle?
    private let cancelPath: String
    private let started = Date()
    init(progress: String, cancel: String) {
        fh = FileHandle(forWritingAtPath: progress)
        _ = try? fh?.seekToEnd()
        cancelPath = cancel
    }
    private func emit(_ s: String) { try? fh?.write(contentsOf: Data((s + "\n").utf8)) }
    func progress(_ f: Double, _ msg: String) { emit("P \(min(max(f, 0), 1)) \(msg)") }
    func log(_ s: String) { emit("L \(s)") }
    func eta(_ seconds: Double) { emit("E \(Int(seconds))") }
    func done() { emit("DONE") }
    func fail(_ s: String) { emit("ERR \(s)") }
    var cancelled: Bool { FileManager.default.fileExists(atPath: cancelPath) }
    func checkCancel() throws { if cancelled { throw HelperError(message: "Cancelled by user") } }
}

enum Helper {
    static func main(_ args: [String]) -> Int32 {
        var o: [String: String] = [:]
        var i = 0
        while i < args.count {
            if args[i] == "--helper" { i += 1; continue }
            if args[i].hasPrefix("--"), i + 1 < args.count { o[String(args[i].dropFirst(2))] = args[i + 1]; i += 2 } else { i += 1 }
        }
        guard let image = o["image"], let disk = o["disk"], let mode = o["mode"],
              let pfile = o["progress"], let cfile = o["cancel"] else { return 2 }
        let rep = Reporter(progress: pfile, cancel: cfile)
        do {
            guard let drive = DiskService.validate(disk) else {
                throw HelperError(message: "\(disk) is not a safe target (must be a whole external drive, not the boot disk)")
            }
            guard let info = ImageInfo.inspect(image) else { throw HelperError(message: "Cannot read image \(image)") }
            guard info.size <= drive.size || mode == "extract" else {
                throw HelperError(message: "Image is larger than the drive")
            }
            rep.log("Target: \(drive.title)")
            switch mode {
            case "dd":
                try ddWrite(image: image, drive: drive, size: info.size, verify: o["verify"] == "1", rep: rep)
            case "extract":
                try extract(image: image, drive: drive,
                            scheme: o["scheme"] ?? "MBR", fs: o["fs"] ?? "FAT32",
                            label: o["label"] ?? "NDAGAFIJO", rep: rep)
            default:
                throw HelperError(message: "Unknown mode \(mode)")
            }
            rep.done()
            return 0
        } catch let e as HelperError {
            rep.fail(e.message); return 1
        } catch let e as FastIO.Failure {
            rep.fail(e.message); return 1
        } catch let e as WimSplit.Failure {
            rep.fail(e.message); return 1
        } catch let e as BiosBoot.Failure {
            rep.fail(e.message); return 1
        } catch let e as Staging.Failure {
            rep.fail(e.message); return 1
        } catch {
            rep.fail(error.localizedDescription); return 1
        }
    }

    // MARK: - DD mode

    static func unmount(_ drive: Drive, _ rep: Reporter) throws {
        rep.progress(0, "Unmounting \(drive.id)...")
        let r = Shell.diskutil(["unmountDisk", "force", drive.id])
        if r.status != 0 { throw HelperError(message: "Could not unmount \(drive.id): \(r.text)") }
    }

    static func errnoMessage(_ what: String) -> HelperError {
        HelperError(message: "\(what): \(String(cString: strerror(errno)))")
    }

    static func ddWrite(image: String, drive: Drive, size: UInt64, verify: Bool, rep: Reporter) throws {
        try unmount(drive, rep)
        let src = open(image, O_RDONLY)
        guard src >= 0 else { throw errnoMessage("Cannot open image") }
        defer { close(src) }
        let dst = open(drive.rawPath, O_WRONLY)
        guard dst >= 0 else { throw errnoMessage("Cannot open \(drive.rawPath)") }
        defer { close(dst) }
        _ = fcntl(dst, F_NOCACHE, 1)
        _ = fcntl(src, F_RDAHEAD, 1)

        let span = verify ? 0.5 : 1.0
        var written: UInt64 = 0
        let started = Date()
        try FastIO.copy(from: src, srcOffset: 0, to: dst, length: size, bufferSize: 8 << 20, depth: 4, padTo: 512,
                        onBytes: { n in
                            written += UInt64(n)
                            let rate = Double(written) / max(Date().timeIntervalSince(started), 0.001) / 1_000_000
                            rep.progress(span * Double(written) / Double(size), String(format: "Writing image... %.0f MB/s", rate))
                        },
                        cancelled: { rep.cancelled })
        fsync(dst)
        rep.log("Wrote \(written) bytes")

        if verify { try verifyWrite(image: image, drive: drive, size: size, rep: rep) }
        _ = Shell.diskutil(["mountDisk", drive.id])
    }

    static func verifyWrite(image: String, drive: Drive, size: UInt64, rep: Reporter) throws {
        let bs = 4 << 20
        var a: UnsafeMutableRawPointer?, b: UnsafeMutableRawPointer?
        guard posix_memalign(&a, 4096, bs) == 0, posix_memalign(&b, 4096, bs) == 0, let ba = a, let bb = b
        else { throw HelperError(message: "Out of memory") }
        defer { free(ba); free(bb) }
        let src = open(image, O_RDONLY), dev = open(drive.rawPath, O_RDONLY)
        guard src >= 0, dev >= 0 else { throw errnoMessage("Cannot reopen for verification") }
        defer { close(src); close(dev) }
        _ = fcntl(dev, F_NOCACHE, 1)

        var done: UInt64 = 0
        while done < size {
            try rep.checkCancel()
            let want = Int(min(UInt64(bs), size - done))
            var got = 0
            while got < want {
                let n = read(src, ba + got, want - got)
                if n <= 0 { throw errnoMessage("Image read error") }
                got += n
            }
            let devLen = (want + 511) & ~511
            var dg = 0
            while dg < devLen {
                let n = read(dev, bb + dg, devLen - dg)
                if n <= 0 { throw errnoMessage("Device read error") }
                dg += n
            }
            if memcmp(ba, bb, want) != 0 { throw HelperError(message: "Verification failed near offset \(done)") }
            done += UInt64(want)
            rep.progress(0.5 + 0.5 * Double(done) / Double(size), "Verifying...")
        }
        rep.log("Verification passed")
    }

    // MARK: - ISO mode (erase, then copy files)

    static func sanitizeLabel(_ s: String, fs: String) -> String {
        if fs == "FAT32" {
            let allowed = Set("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_- ")
            let t = String(s.uppercased().filter { allowed.contains($0) }.prefix(11))
            return t.isEmpty ? "NDAGAFIJO" : t
        }
        let t = String(s.filter { !"/\\:*?\"<>|".contains($0) }.prefix(15))
        return t.isEmpty ? "NDAGAFIJO" : t
    }

    static func extract(image: String, drive: Drive, scheme: String, fs: String, label: String, rep: Reporter) throws {
        guard ["MBR", "GPT"].contains(scheme), ["FAT32", "ExFAT"].contains(fs) else {
            throw HelperError(message: "Unsupported scheme/file system")
        }
        try unmount(drive, rep)

        rep.progress(0.01, "Mounting image...")
        let attach = Shell.plist("/usr/bin/hdiutil", ["attach", "-readonly", "-nobrowse", "-noverify", "-plist", image])
        guard let ents = attach?["system-entities"] as? [[String: Any]],
              let mount = ents.compactMap({ $0["mount-point"] as? String }).first
        else { throw HelperError(message: "Could not mount the ISO image") }
        defer { _ = Shell.run("/usr/bin/hdiutil", ["detach", "-force", mount]) }
        let srcRoot = URL(fileURLWithPath: mount)

        // Scan before erasing anything, so impossible jobs fail while the drive is still intact.
        rep.progress(0.03, "Scanning image...")
        let scanned = try TreeCopy.scan(srcRoot, rep: rep)
        let tooBig = scanned.files.filter {
            fs == "FAT32" && $0.size > TreeCopy.fat32Limit && $0.rel.lowercased() != "sources/install.wim"      // install.wim is split
        }.map { $0.rel }
        if !tooBig.isEmpty {
            throw HelperError(message: "FAT32 cannot hold files over 4 GB (\(tooBig.joined(separator: ", "))). The drive was not modified.")
        }
        if scanned.total > drive.size { throw HelperError(message: "The image contents do not fit on the drive. The drive was not modified.") }

        let vol = sanitizeLabel(label, fs: fs)
        rep.progress(0.05, "Formatting as \(fs) (\(scheme))...")
        let er = Shell.diskutil(["eraseDisk", fs, vol, scheme, drive.id])
        rep.log(er.text.trimmingCharacters(in: .whitespacesAndNewlines))
        if er.status != 0 { throw HelperError(message: "Format failed") }

        let cores = ProcessInfo.processInfo.activeProcessorCount
        let threads = Int(ProcessInfo.processInfo.environment["NDAGA_THREADS"] ?? "") ?? max(2, min(8, cores))
        var staged = false
        if fs == "FAT32", !Staging.disabled, Staging.hasRoom(for: scanned.total), let part = dataVolumePartition(drive.id) {
            rep.log("Using the fast path: parallel staging (\(threads) threads), then one sequential write")
            _ = Shell.diskutil(["unmountDisk", drive.id])
            try Staging.writeFAT32(scan: scanned, drive: drive, partition: part, label: vol, threads: threads, rep: rep)
            staged = true
        }
        if !staged {
            guard let dest = dataVolumeMountPoint(drive.id) else { throw HelperError(message: "Cannot find the new volume") }
            rep.log("Copying to \(dest)")
            try TreeCopy.copy(scanned, dest: dest, splitLargeWim: fs == "FAT32", threads: 1, rep: rep, progress: { done, name in
                rep.progress(0.1 + 0.88 * Double(done) / Double(max(scanned.total, 1)), "Copying \(name)")
            })
            rep.progress(0.99, "Flushing...")
            sync()
        }
        rep.log("Copied \(scanned.total) bytes")
        let hasBootmgr = scanned.hasBootmgr

        // Windows media also needs boot code for legacy BIOS (UEFI boots from the files alone).
        if hasBootmgr && scheme == "MBR" && fs == "FAT32", let part = dataVolumePartition(drive.id) {
            rep.progress(0.995, "Installing BIOS boot code...")
            do {
                try BiosBoot.installWindows(disk: drive.id, partition: part)
                rep.log("Installed Windows BIOS boot code (MBR + FAT32 boot record)")
            } catch let e as BiosBoot.Failure {                // files are intact, so UEFI boot still works
                rep.log("Warning: BIOS boot code was not installed (\(e.message)). UEFI boot is unaffected.")
                BiosBoot.remount(drive.id)
            }
        }
    }

    /// Device identifier ("diskNs1") of the data partition on a freshly erased disk.
    static func dataVolumePartition(_ disk: String) -> String? {
        guard let list = Shell.diskutilPlist(["list", "-plist", disk]),
              let all = list["AllDisksAndPartitions"] as? [[String: Any]],
              let parts = all.first?["Partitions"] as? [[String: Any]]
        else { return nil }
        return parts.first { !(($0["MountPoint"] as? String) ?? "").isEmpty }?["DeviceIdentifier"] as? String
    }

    /// Mount point of the data partition on a freshly erased disk (skips the hidden EFI partition).
    static func dataVolumeMountPoint(_ disk: String) -> String? {
        guard let list = Shell.diskutilPlist(["list", "-plist", disk]),
              let all = list["AllDisksAndPartitions"] as? [[String: Any]],
              let parts = all.first?["Partitions"] as? [[String: Any]]
        else { return nil }
        return parts.compactMap { $0["MountPoint"] as? String }.first { !$0.isEmpty }
    }
}
