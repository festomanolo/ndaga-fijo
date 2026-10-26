/*
 * Ndaga Fijo - drive enumeration and safety checks
 * Based on Rufus, Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 */
import Foundation

struct Drive: Identifiable, Hashable {
    let id: String          // "disk4"
    let name: String
    let size: UInt64
    let bus: String
    let removable: Bool

    var rawPath: String { "/dev/r\(id)" }
    var title: String {
        let sz = ByteCountFormatter.string(fromByteCount: Int64(size), countStyle: .file)
        return "\(name.isEmpty ? "USB Drive" : name) [\(sz)] (\(id))"
    }
}

enum DiskService {
    /// Disk images (hdiutil attach) are only offered for development/testing, so they can never
    /// be confused with a real device by accident.
    static var allowDiskImages: Bool { ProcessInfo.processInfo.environment["NDAGA_ALLOW_DISK_IMAGES"] == "1" }

    /// Every whole disk that holds the running system. On APFS, "/" lives on a synthesized
    /// container, so the physical disk(s) behind it are included as well. None may be a target.
    static func bootDisks() -> Set<String> {
        guard let info = Shell.diskutilPlist(["info", "-plist", "/"]) else { return [] }
        var disks = Set<String>()
        func whole(_ id: String) -> String {
            id.range(of: #"^disk[0-9]+"#, options: .regularExpression).map { String(id[$0]) } ?? id
        }
        if let p = info["ParentWholeDisk"] as? String { disks.insert(p) }
        if let c = info["APFSContainerReference"] as? String { disks.insert(whole(c)) }
        for store in info["APFSPhysicalStores"] as? [[String: Any]] ?? [] {
            if let s = store["APFSPhysicalStore"] as? String { disks.insert(whole(s)) }
        }
        return disks
    }

    /// Returns a Drive only if `disk` is a whole, external (or removable) physical disk
    /// that is not the boot disk.
    static func validate(_ disk: String) -> Drive? {
        guard disk.range(of: #"^disk[0-9]+$"#, options: .regularExpression) != nil,
              let info = Shell.diskutilPlist(["info", "-plist", disk]),
              (info["WholeDisk"] as? Bool) == true
        else { return nil }

        let bus = info["BusProtocol"] as? String ?? ""
        let isImage = bus == "Disk Image"
        if isImage { guard allowDiskImages else { return nil } }
        else {
            let internalDisk = (info["Internal"] as? Bool) ?? true
            let removable = (info["RemovableMediaOrExternalDevice"] as? Bool) ?? (info["Removable"] as? Bool) ?? false
            if internalDisk && !removable { return nil }
            if (info["VirtualOrPhysical"] as? String) == "Virtual" { return nil }
        }
        if bootDisks().contains(disk) { return nil }

        let size = (info["Size"] as? UInt64) ?? (info["TotalSize"] as? UInt64) ?? UInt64((info["Size"] as? Int) ?? 0)
        guard size > 0 else { return nil }
        let flash = (info["Removable"] as? Bool) == true || (info["RemovableMedia"] as? Bool) == true
        return Drive(id: disk, name: info["MediaName"] as? String ?? "", size: size, bus: bus,
                     removable: flash || isImage)
    }

    /// Fixed external disks (SSDs, backup drives) are hidden unless `includeFixed` is set.
    static func listDrives(includeFixed: Bool = false) -> [Drive] {
        let args = allowDiskImages ? ["list", "-plist"] : ["list", "-plist", "external", "physical"]
        guard let list = Shell.diskutilPlist(args),
              let ids = list["WholeDisks"] as? [String]
        else { return [] }
        return ids.compactMap(validate).filter { includeFixed || $0.removable }
    }
}
