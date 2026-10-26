/*
 * Rufus for macOS - user options
 * Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 */
import Foundation

enum PartitionScheme: String, CaseIterable, Identifiable {
    case mbr = "MBR", gpt = "GPT"
    var id: String { rawValue }
    /// Extracted drives only carry /EFI/BOOT files; diskutil installs no MBR/PBR boot code.
    var target: String { "UEFI (non-CSM)" }
}

enum FileSystemChoice: String, CaseIterable, Identifiable {
    case fat32 = "FAT32", exfat = "ExFAT"
    var id: String { rawValue }
}

enum WriteMode: String, CaseIterable, Identifiable {
    case auto, dd, extract
    var id: String { rawValue }
    var title: String {
        switch self {
        case .auto: return "Automatic"
        case .dd: return "DD Image mode"
        case .extract: return "ISO mode (extract files)"
        }
    }
}
