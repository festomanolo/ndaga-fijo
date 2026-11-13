#!/bin/sh
# Builds the two NTFS tools Ndaga Fijo bundles into OUT (default ../build/tools), as universal binaries:
#   mkntfs     - from ntfs-3g (GPL v2+), formats the staged NTFS volume;
#   ndaga-ntfs - this crate (VOLUMA's NTFS driver), copies the files and streams the image to the drive.
# Set NTFS3G_TOOLS=1 to also keep ntfsresize/ntfsls/ntfscat (used by the test script) in OUT.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:-$HERE/../build/tools}"
mkdir -p "$OUT"; OUT="$(cd "$OUT" && pwd)"
VER=2022.10.3
SHA=f20e36ee68074b845e3629e6bced4706ad053804cbaf062fbae60738f854170c
CACHE="$HERE/.cache"; mkdir -p "$CACHE"
TGZ="$CACHE/ntfs-3g_ntfsprogs-$VER.tgz"

NEED=0; [ -x "$OUT/mkntfs" ] || NEED=1
if [ "${NTFS3G_TOOLS:-0}" = 1 ] && [ ! -x "$OUT/ntfsresize" ]; then NEED=1; fi
if [ "$NEED" = 1 ]; then
  [ -f "$TGZ" ] || curl -fsSL -o "$TGZ" "https://tuxera.com/opensource/ntfs-3g_ntfsprogs-$VER.tgz"
  echo "$SHA  $TGZ" | shasum -a 256 -c - >/dev/null || { echo "checksum mismatch for $TGZ" >&2; rm -f "$TGZ"; exit 1; }
  for ARCH in arm64 x86_64; do
    W="$CACHE/build-$ARCH"; rm -rf "$W"; mkdir -p "$W"
    tar xzf "$TGZ" -C "$W" --strip-components=1
    (cd "$W" && CFLAGS="-O2 -arch $ARCH -mmacosx-version-min=13.0" ./configure --host="$ARCH-apple-darwin" \
        --disable-ntfs-3g --enable-ntfsprogs --disable-library --disable-mount-helper --disable-crypto \
        --disable-extras --without-uuid --without-hd --disable-nfconv --disable-shared --enable-static >/dev/null \
      && make -j"$(sysctl -n hw.ncpu)" >/dev/null)
  done
  TOOLS=mkntfs; [ "${NTFS3G_TOOLS:-0}" = 1 ] && TOOLS="mkntfs ntfsresize ntfsls ntfscat ntfsinfo"
  for t in $TOOLS; do
    lipo -create -output "$OUT/$t" "$CACHE/build-arm64/ntfsprogs/$t" "$CACHE/build-x86_64/ntfsprogs/$t"
  done
fi

cd "$HERE"
cargo build --release --target aarch64-apple-darwin --target x86_64-apple-darwin
lipo -create -output "$OUT/ndaga-ntfs" target/aarch64-apple-darwin/release/ndaga-ntfs target/x86_64-apple-darwin/release/ndaga-ntfs
echo "NTFS tools in $OUT"
