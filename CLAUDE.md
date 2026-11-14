# Ndaga Fijo

A native macOS (SwiftUI) bootable-USB creator derived from Rufus. All Mac work lives in `macos/`; the
Windows Rufus sources (`src/`, `.vs/`, `.mingw/`, autotools files) are upstream and stay untouched.
`macos/README.md` is the user-facing description of how everything works; keep it in sync with behaviour changes.

## Build and test

- Full app: `cd macos && ./build-app.sh && open build/NdagaFijo.app` (universal Swift binary + NTFS tools, ad-hoc signed).
