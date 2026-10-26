## Ndaga Fijo 1.0.0

A native macOS bootable-USB creator (Apple Silicon + Intel, macOS 13+), based on Rufus.

**Highlights**
- Windows-ready: an `install.wim` over 4 GB is split into `.swm` parts automatically for FAT32 drives.
- Fast: ISO contents are staged on your SSD and streamed to the drive in one sequential pass. On a USB stick, the same 712 MB / 6,959-file Windows XP ISO took **2 min 26 s** versus **34 min 10 s** for a plain `cp -R` (raw sequential limit of that stick: 1 min 7 s).
- Time remaining and elapsed time shown while writing.
- BIOS boot code for Windows media (MBR + FAT32); UEFI boots from the files.
- Safe by design: only removable drives are listed, the disk macOS runs from is refused.
- Liquid Glass interface on macOS 26, with a material fallback on macOS 13-15.

**Install**
- Download the DMG or ZIP below, or `pip install ndaga-fijo` then `ndaga-fijo`.
- The app is ad-hoc signed, not notarized. If macOS blocks it: right-click → Open, or System Settings → Privacy & Security → Open Anyway. (`pip install` avoids the prompt.)

**Not verified yet:** booting the written drives on real PCs (UEFI and legacy BIOS), and Windows Setup reading the split `.swm` files. Please report results in an issue.

**Not supported:** NTFS/UEFI:NTFS, Windows To Go, Windows setup customization, bad-block checks, BIOS boot for non-hybrid Linux ISOs.

SHA-256 checksums are in `SHA256SUMS.txt`.
