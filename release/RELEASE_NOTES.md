## Ndaga Fijo 1.1.0

**New: NTFS drives, so Windows images with files over 4 GB work**
- Images with a file FAT32 cannot hold (such as a Windows 10/11 `install.esd` over 4 GB) are now written as NTFS. This
  happens automatically, as Rufus does, or you can pick NTFS in the menu. Previously these failed with
  "FAT32 cannot hold files over 4 GB".
- Same layout as Rufus: an NTFS partition plus a 1 MB **UEFI:NTFS** partition (Secure Boot signed), so UEFI PCs boot
  from NTFS. On MBR, the Windows 7 MBR and NTFS boot record are installed for legacy BIOS.
- Formatted with `mkntfs` from ntfs-3g; files written with VOLUMA's NTFS driver. Both are bundled, nothing to install.
- Fast: staged on your SSD, then only the used clusters are streamed to the drive. A 5.5 GB Windows 10 ISO (910 files)
  was written in **88 s** to a disk image.
- Checked on disk images: every file byte-identical when read back by macOS' NTFS driver, and ntfs-3g's
  consistency check (`ntfsresize -i -f`) reports no errors, for both MBR and GPT.

**Not verified yet:** booting NTFS drives on real PCs (UEFI:NTFS and legacy BIOS), and Windows' own `chkdsk` on
them. Please report results in an issue. macOS mounts NTFS drives read-only; that is normal.

**Install**
- Download the DMG or ZIP, or `pip install --upgrade ndaga-fijo` then `ndaga-fijo`.
- The app is ad-hoc signed, not notarized. If macOS blocks it: right-click → Open, or System Settings → Privacy & Security → Open Anyway. (`pip install` avoids the prompt.)

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
