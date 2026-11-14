# Ndaga Fijo

A native macOS (SwiftUI) bootable-USB creator derived from Rufus. All Mac work lives in `macos/`; the
Windows Rufus sources (`src/`, `.vs/`, `.mingw/`, autotools files) are upstream and stay untouched.
`macos/README.md` is the user-facing description of how everything works; keep it in sync with behaviour changes.

## Build and test

- Full app: `cd macos && ./build-app.sh && open build/NdagaFijo.app` (universal Swift binary + NTFS tools, ad-hoc signed).
- NTFS tools only: `macos/ntfs/build.sh [OUT]` (default `macos/build/tools`); first run downloads ntfs-3g once into `ntfs/.cache`.
- Rust unit tests: `cd macos/ntfs && cargo test --release` (includes the FastDevice model test).
- If `swift build` fails with "precompiled file ... was compiled with module cache path .../rufus-master/...",
  the project folder was renamed: delete the `ModuleCache` dirs under `macos/.build` (or all of `.build`).
- Never test writes against a real drive. Use disk images: `NDAGA_ALLOW_DISK_IMAGES=1` lists
  `hdiutil attach -nomount` images in the app. For `ndaga-ntfs`, attach a file with
  `hdiutil attach -nomount -imagekey diskimage-class=CRawDiskImage disk.img` and target `/dev/rdiskN`;
  check results with the ntfs-3g tools (`NTFS3G_TOOLS=1 ntfs/build.sh` keeps `ntfsinfo`/`ntfsls`/`ntfscat`).

## Layout

- `macos/Sources/NdagaFijo/`: `Helper.swift` (root helper: DD and ISO modes, FS auto-pick: FAT32 -> NTFS when a
  file is over 4 GB, except `install.wim`, which `WimSplit.swift` splits), `NtfsMedia.swift` (NTFS + UEFI:NTFS
  layout, partition tables written last), `Staging.swift` (staged FAT32 writer and staging-location choice),
  `FastIO.swift` (pipelined DD copy), `BootCode.swift`/`BiosBoot.swift` (ms-sys boot records).
- `macos/ntfs/`: `ndaga-ntfs` Rust CLI (`copy`, `stream`; prints `P done total name` progress and `L text` log lines,
  which `NtfsMedia.runTool` parses). `src/fastdev.rs` is the block layer under the driver.
  `vendor/` holds VOLUMA's NTFS driver crates (MIT): avoid changing them; put new behaviour in `src/`.

## NTFS write path (performance-critical)

- Staged (normal): mkntfs formats a sparse image, `ndaga-ntfs copy` fills it, `ndaga-ntfs stream` writes only
  in-use clusters to the drive. The image goes in `/private/var/tmp`, or on the ISO's own volume if that is APFS
  (needed for sparse files) and the system disk lacks room (`Staging.directory(for:near:)`).
