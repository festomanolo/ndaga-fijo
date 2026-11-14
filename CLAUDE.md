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

