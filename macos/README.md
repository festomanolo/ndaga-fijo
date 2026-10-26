# Rufus for macOS

A native macOS (SwiftUI) port of the Rufus workflow: pick a removable drive, pick an ISO/IMG,
choose the format options, and write. The Windows sources in `../src` are untouched.

## Build and run

    ./build-app.sh && open build/Rufus.app

Requires Xcode command line tools (Swift 5.9+), macOS 13+.

## How it works

* **Drives:** only whole removable physical disks are listed (fixed external drives need "List USB hard drives"); disks backing the running system, including the APFS physical store, are always refused.
  The privileged helper re-checks this before touching anything.
* **DD mode** (hybrid ISOs and raw images): block-for-block write to `/dev/rdiskN`, optional read-back verify.
* **ISO mode** (plain ISOs): drive is erased as MBR or GPT with FAT32 or ExFAT, then the ISO contents are copied.
  Windows ISOs work: an `install.wim` over 4 GB is split into `install.swm`, `install2.swm`... (compressed data is copied
  untouched, only headers are rewritten), which Windows Setup reads natively. With MBR + FAT32 and a `bootmgr` on the image,
  the Windows 7 MBR and FAT32 NT boot record (from `src/ms-sys`) are installed so legacy BIOS can boot too. Other extracted
  ISOs boot via UEFI only.
* **Speed:** a reader thread keeps a ring of 8 MB aligned buffers full while the writer drains them, using uncached
  transfers on the raw device. Writes show a live MB/s figure. Verify reads back at full speed instead of re-decoding.
* Not yet verified: booting on real hardware (UEFI and BIOS), the admin-prompt path end to end, and writes to real USB drives.
* Writing needs root: the app asks for your password once per job (standard macOS prompt) and runs only its
  own small helper mode (`Rufus --helper`) as root.
* Checksums: MD5, SHA-1, SHA-256, SHA-512.

## Not ported (Windows-only or not feasible)

Windows To Go, Windows install customization (OOBE/TPM bypass), NTFS/ReFS/UDF formatting, UEFI:NTFS, syslinux BIOS boot for non-hybrid Linux ISOs,
bad-block checks, FreeDOS/MS-DOS, VHD/FFU images, persistent Linux partitions, Microsoft ISO downloader,
compressed image support, localization.

## Testing safely

Set `RUFUS_ALLOW_DISK_IMAGES=1` to also list `hdiutil attach -nomount` disk images, and test against those:

    hdiutil create -size 64m -layout NONE test.dmg && hdiutil attach -nomount test.dmg
    RUFUS_ALLOW_DISK_IMAGES=1 open build/Rufus.app

Licensed under GPL v3 or later, like Rufus.
