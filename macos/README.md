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
  Extracted drives boot via UEFI only (no BIOS boot code is written).
  FAT32 cannot hold files over 4 GB, which includes the `install.wim` of current Windows 10/11 ISOs. This is detected
  before the drive is erased and refused: **Windows ISOs are not supported yet** (splitting the WIM via `src/wimlib` is the planned fix).
* Not yet verified: the admin-prompt path end to end, writes to real USB hardware, and booting the result on real machines.
* Writing needs root: the app asks for your password once per job (standard macOS prompt) and runs only its
  own small helper mode (`Rufus --helper`) as root.
* Checksums: MD5, SHA-1, SHA-256, SHA-512.

## Not ported (Windows-only or not feasible)

Windows To Go, Windows install customization (OOBE/TPM bypass), NTFS/ReFS/UDF formatting, UEFI:NTFS,
bad-block checks, FreeDOS/MS-DOS, VHD/FFU images, persistent Linux partitions, Microsoft ISO downloader,
compressed image support, localization.

## Testing safely

Set `RUFUS_ALLOW_DISK_IMAGES=1` to also list `hdiutil attach -nomount` disk images, and test against those:

    hdiutil create -size 64m -layout NONE test.dmg && hdiutil attach -nomount test.dmg
    RUFUS_ALLOW_DISK_IMAGES=1 open build/Rufus.app

Licensed under GPL v3 or later, like Rufus.
