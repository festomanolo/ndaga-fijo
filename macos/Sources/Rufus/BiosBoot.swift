/*
 * Rufus for macOS - BIOS boot code for Windows installation drives
 * Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 *
 * diskutil formats the drive but writes no boot code. For an MBR + FAT32 drive that carries
 * Windows' bootmgr, this installs the Windows 7 MBR and the FAT32 NT boot record (from ms-sys,
 * the same code Rufus uses), and marks the partition active so legacy BIOS can boot it.
 */
import Foundation

enum BiosBoot {
    struct Failure: Error { let message: String }

    /// Read-modify-write of arbitrary bytes on a raw device, which only accepts whole sectors.
    private static func patch(_ fd: Int32, at offset: Int, _ bytes: [UInt8]) throws {
        let start = offset / 512 * 512
        let end = (offset + bytes.count + 511) / 512 * 512
        var buf = [UInt8](repeating: 0, count: end - start)
        guard pread(fd, &buf, buf.count, off_t(start)) == buf.count else { throw Failure(message: "Cannot read boot sectors") }
        buf.replaceSubrange((offset - start)..<(offset - start + bytes.count), with: bytes)
        guard pwrite(fd, buf, buf.count, off_t(start)) == buf.count else { throw Failure(message: "Cannot write boot sectors") }
    }

    /// `disk` is "diskN", `partition` is "diskNs1".
    static func installWindows(disk: String, partition: String) throws {
        guard let pinfo = Shell.diskutilPlist(["info", "-plist", partition]),
              let startBytes = (pinfo["PartitionMapPartitionOffset"] as? UInt64) ?? (pinfo["PartitionMapPartitionOffset"] as? Int).map(UInt64.init)
        else { throw Failure(message: "Cannot read the partition layout") }

        let r = Shell.diskutil(["unmountDisk", disk])
        guard r.status == 0 else { throw Failure(message: "Cannot unmount for boot code: \(r.text)") }

        // 1. FAT32 NT boot record (sectors 0-2, backup at 6-8, loader at sector 12).
        let pfd = open("/dev/r\(partition)", O_RDWR)
        guard pfd >= 0 else { throw Failure(message: "Cannot open /dev/r\(partition): \(String(cString: strerror(errno)))") }
        var s0 = [UInt8](repeating: 0, count: 512)
        guard pread(pfd, &s0, 512, 0) == 512, Array(s0[0x52..<0x5A]) == Array("FAT32   ".utf8) else {
            throw Failure(message: "The partition is not FAT32")
        }
        // The loader reads with the BPB's hidden-sector count, so it must equal the real partition start.
        let hidden = UInt32(startBytes / 512)
        for i in 0..<4 { s0[0x1C + i] = UInt8((hidden >> (8 * UInt32(i))) & 0xFF) }
        guard pwrite(pfd, s0, 512, 0) == 512 else { throw Failure(message: "Cannot update the BPB") }

        try patch(pfd, at: 0x0, BootCode.fat32Br0x0)
        try patch(pfd, at: 0x52, BootCode.fat32NtBr0x52)
        try patch(pfd, at: 0x3f0, BootCode.fat32NtBr0x3f0)
        try patch(pfd, at: 0x1800, BootCode.fat32NtBr0x1800)
        var boot = [UInt8](repeating: 0, count: 1536)
        guard pread(pfd, &boot, 1536, 0) == 1536 else { throw Failure(message: "Cannot read back the boot record") }
        guard pwrite(pfd, boot, 1536, 6 * 512) == 1536 else { throw Failure(message: "Cannot write the backup boot record") }
        fsync(pfd)
        close(pfd)                                      // the whole-disk node is busy while this is open

        // 2. Windows 7 MBR code; keep the disk signature and partition table, mark partition 1 active.
        let dfd = open("/dev/r\(disk)", O_RDWR)
        guard dfd >= 0 else { throw Failure(message: "Cannot open /dev/r\(disk): \(String(cString: strerror(errno)))") }
        defer { close(dfd) }
        var mbr = [UInt8](repeating: 0, count: 512)
        guard pread(dfd, &mbr, 512, 0) == 512 else { throw Failure(message: "Cannot read the MBR") }
        mbr.replaceSubrange(0..<BootCode.mbrWin7.count, with: BootCode.mbrWin7)
        for e in 0..<4 { mbr[446 + e * 16] = e == 0 ? 0x80 : 0x00 }
        mbr[510] = 0x55; mbr[511] = 0xAA
        guard pwrite(dfd, mbr, 512, 0) == 512 else { throw Failure(message: "Cannot write the MBR") }
        fsync(dfd)

        remount(disk)
    }

    /// DiskArbitration needs a moment after raw writes; retry until the volume shows up.
        _ = Shell.diskutil(["mountDisk", disk])
    }
}
