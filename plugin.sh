#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/cursor-sub.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/cursor-sub" ;;
    *) rel="x86_64-apple-darwin/cursor-sub" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/cursor-sub" ;;
    *) rel="x86_64-unknown-linux-musl/cursor-sub" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/cursor-sub.exe" "$here/target/release/cursor-sub" "$here/target/debug/cursor-sub.exe" "$here/target/debug/cursor-sub"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "cursor-sub binary not found in $here" >&2
exit 1
