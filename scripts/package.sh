#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Package one built binary into the archive the installers expect.
#
#   scripts/package.sh --version 0.1.0 --target x86_64-pc-windows-msvc \
#       --binary target/release/kiomon.exe --out dist
#
# This exists as a script rather than as twenty lines inside the release workflow for one
# reason: the workflow is the only thing that would run it, and a packaging step that only
# CI can execute is a step whose failures are discovered at release time. `install.sh`
# against an archive this script made is a test that runs on a laptop.
#
# The archive's name is a promise: `kiomon-<target>.<ext>`, no version in it, so
# `releases/latest/download/<name>` is a valid URL. The version goes in the directory
# inside it, where a user who unpacks it sees it.
#
# `bash` rather than `sh`, unlike install.sh: this runs on a runner we control, `set -o
# pipefail` is worth having, and it is never piped from the internet.
set -euo pipefail

VERSION=""
TARGET=""
BINARY=""
OUT="dist"

usage() {
    cat <<'EOF'
Usage: scripts/package.sh --version <V> --target <TRIPLE> --binary <PATH> [--out <DIR>]

Builds <out>/kiomon-<triple>.tar.gz, or .zip for a Windows target, containing
kiomon-<V>/ with the binary, the licence, the notice, and the README.
EOF
}

die() {
    printf 'package.sh: %s\n' "$*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2:-}"; shift 2 ;;
        --target) TARGET="${2:-}"; shift 2 ;;
        --binary) BINARY="${2:-}"; shift 2 ;;
        --out) OUT="${2:-}"; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; die "unknown argument: $1" ;;
    esac
done

[ -n "$VERSION" ] || die "--version is required (the tag without its leading v)"
[ -n "$TARGET" ] || die "--target is required"
[ -n "$BINARY" ] || die "--binary is required"
[ -f "$BINARY" ] || die "no binary at $BINARY"

# The file the user gets is named `kiomon` and nothing else: an installer that has to guess
# between `kiomon` and `kiomon.exe` is one that breaks on the next rename.
case "$TARGET" in
    *-windows-*) NAME="kiomon.exe" ;;
    *) NAME="kiomon" ;;
esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$OUT"
# Absolute, once: the archive is made from inside the staging directory, so a relative
# `--out` would otherwise name a path that stops meaning what it said halfway through.
OUT_ABS="$(cd "$OUT" && pwd)"
STAGE="${OUT_ABS}/stage/kiomon-${VERSION}"
rm -rf "${OUT_ABS}/stage"
mkdir -p "$STAGE"
cp "$BINARY" "$STAGE/$NAME"
for file in LICENSE NOTICE README.md; do
    [ -f "$ROOT/$file" ] || die "missing $ROOT/$file"
    cp "$ROOT/$file" "$STAGE/"
done

case "$TARGET" in
    *-windows-*)
        ARCHIVE="${OUT_ABS}/kiomon-${TARGET}.zip"
        rm -f "$ARCHIVE"
        if command -v zip >/dev/null 2>&1; then
            (cd "${OUT_ABS}/stage" && zip -qr "$ARCHIVE" "kiomon-${VERSION}")
        elif command -v 7z >/dev/null 2>&1; then
            # The windows-latest runner has 7-Zip and not `zip`.
            (cd "${OUT_ABS}/stage" && 7z a -tzip -bso0 -bsp0 "$ARCHIVE" "kiomon-${VERSION}")
        elif command -v powershell >/dev/null 2>&1; then
            powershell -NoProfile -Command "Compress-Archive -Path '${STAGE}' -DestinationPath '${ARCHIVE}' -Force"
        else
            die "no zip, 7z or powershell to make $ARCHIVE"
        fi
        ;;
    *)
        ARCHIVE="${OUT_ABS}/kiomon-${TARGET}.tar.gz"
        rm -f "$ARCHIVE"
        # `--owner=0 --group=0 --mtime` would make the archive byte-identical between runs;
        # it is left out because GNU and BSD tar disagree about the spelling, and a
        # reproducible archive nobody checks is worse than an honest timestamp.
        tar -czf "$ARCHIVE" -C "${OUT_ABS}/stage" "kiomon-${VERSION}"
        ;;
esac

[ -s "$ARCHIVE" ] || die "$ARCHIVE is empty"
rm -rf "${OUT_ABS}/stage"
printf '%s\n' "$ARCHIVE"
