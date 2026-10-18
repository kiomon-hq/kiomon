#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Install `kiomon` with one command:
#
#   curl -fsSL https://raw.githubusercontent.com/kiomon-hq/kiomon/main/install.sh | sh
#
# Or, to see what it would do first — the right way to run a script from the internet:
#
#   curl -fsSLO https://raw.githubusercontent.com/kiomon-hq/kiomon/main/install.sh
#   less install.sh
#   sh install.sh --dry-run
#
# What it does, in order: work out which archive this machine needs, download it and the
# `SHA256SUMS` file from the same release, verify one against the other, extract the binary
# into a directory on your `PATH`, and print the version it installed. It needs `curl` or
# `wget`, `tar`, and one of `sha256sum`/`shasum`/`openssl` — all of which are in a base
# install on every system that has these binaries.
#
# Deliberately POSIX `sh` and not `bash`: `sh` is what `#!/bin/sh` promises, and this script
# is piped into an interpreter whose flags the user has not read. Every construct below is
# in POSIX 1003.1; `local` is not, and is avoided. So is `[ … ] && assignment`, which under
# `set -e` is a script that exits when the test happens to be false.

set -eu

REPO="kiomon-hq/kiomon"
DEFAULT_BASE="https://github.com/${REPO}/releases"

# Assets are named without a version — `kiomon-x86_64-unknown-linux-gnu.tar.gz` — because
# `releases/latest/download/<name>` resolves the newest release without asking the API for
# its tag, which would need a rate-limited request to do what a static path can do. The
# version lives inside the archive, where the binary can also report it.
BASE="${KIOMON_RELEASES_URL:-$DEFAULT_BASE}"
VERSION="latest"
DIR="${KIOMON_INSTALL_DIR:-$HOME/.local/bin}"
TARGET=""
DRY_RUN=0
VERIFY=1

usage() {
    cat <<'EOF'
Install kiomon.

Usage: install.sh [options]

  --version <TAG>    Release tag to install, e.g. v0.1.0. Default: the latest release.
  --dir <PATH>       Where to put the binary. Default: $KIOMON_INSTALL_DIR or ~/.local/bin.
  --target <TRIPLE>  Install for this target instead of the machine's own, e.g.
                     x86_64-unknown-linux-gnu to stage a binary for a container.
  --no-verify        Skip the checksum check. Not recommended: it is the only thing that
                     distinguishes a truncated download from a binary.
  --dry-run          Print what would be downloaded and where it would go, then stop.
  -h, --help         This text.

Environment: KIOMON_INSTALL_DIR (default directory), KIOMON_RELEASES_URL (release host,
useful for a mirror or an air-gapped install), KIOMON_VERSION (same as --version).
EOF
}

die() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

note() {
    printf '%s\n' "$*"
}

# Arguments, left to right. `--flag=value` is not supported on purpose: one spelling means
# one thing to explain when it goes wrong.
while [ $# -gt 0 ]; do
    case "$1" in
        --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION="$2"; shift 2 ;;
        --dir) [ $# -ge 2 ] || die "--dir needs a value"; DIR="$2"; shift 2 ;;
        --target) [ $# -ge 2 ] || die "--target needs a value"; TARGET="$2"; shift 2 ;;
        --no-verify) VERIFY=0; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown argument: $1" ;;
    esac
done

if [ -n "${KIOMON_VERSION:-}" ]; then
    VERSION="$KIOMON_VERSION"
fi

# ── which build does this machine need ─────────────────────────────────────────────
#
# Two functions, because the answer is wanted twice for different reasons: `host_target`
# is silent for a host with no published build (so `--target` can be used from one), and
# `detect_target` is the one that explains the situation and stops.
host_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "${os}/${arch}" in
        Linux/x86_64|Linux/amd64) printf '%s' "x86_64-unknown-linux-gnu" ;;
        Linux/aarch64|Linux/arm64) printf '%s' "aarch64-unknown-linux-gnu" ;;
        Darwin/arm64|Darwin/aarch64) printf '%s' "aarch64-apple-darwin" ;;
        Darwin/x86_64) printf '%s' "x86_64-apple-darwin" ;;
        *) printf '%s' "" ;;
    esac
}

detect_target() {
    host=$(host_target)
    if [ -n "$host" ]; then
        printf '%s' "$host"
        return
    fi
    os=$(uname -s)
    case "$os" in
        Linux|Darwin) die "no build for ${os} on $(uname -m). Build from source: cargo install --path crates/kiomon-cli" ;;
        *) die "no build for ${os}. On Windows use install.ps1; otherwise build from source: cargo install --path crates/kiomon-cli" ;;
    esac
}

HOST=$(host_target)
if [ -z "$TARGET" ]; then
    TARGET=$(detect_target)
fi
case "$TARGET" in
    *-pc-windows-msvc) die "$TARGET is a Windows target; run install.ps1 in PowerShell instead" ;;
esac

# A glibc-linked binary on musl (Alpine, distroless) fails at run time with "not found",
# which reads as a vanished file and not as a missing loader. Say it here, where the user
# is already reading about their system, rather than in a shell error afterwards.
if [ "$TARGET" = "$HOST" ] && [ "$(uname -s)" = "Linux" ]; then
    if ldd --version 2>&1 | grep -qi musl; then
        note "warning: this is a musl system and the published Linux builds are glibc-linked."
        note "warning: build from source instead: cargo install --path crates/kiomon-cli"
    fi
fi

ASSET="kiomon-${TARGET}.tar.gz"
if [ "$VERSION" = "latest" ]; then
    URL="${BASE}/latest/download/${ASSET}"
    SUMS_URL="${BASE}/latest/download/SHA256SUMS"
else
    URL="${BASE}/download/${VERSION}/${ASSET}"
    SUMS_URL="${BASE}/download/${VERSION}/SHA256SUMS"
fi

note "kiomon ${VERSION} for ${TARGET}"
note "  from   ${URL}"
note "  to     ${DIR}/kiomon"

if [ "$DRY_RUN" = "1" ]; then
    note "  sums   ${SUMS_URL}"
    note "dry run: nothing downloaded, nothing written"
    exit 0
fi

# ── fetch, one way or the other ────────────────────────────────────────────────────
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --retry 2 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    die "needs curl or wget to download the release"
fi
command -v tar >/dev/null 2>&1 || die "needs tar to unpack the release"

TMP=$(mktemp -d 2>/dev/null || mktemp -d -t kiomon) || die "could not create a temporary directory"
trap 'rm -rf "$TMP"' EXIT INT TERM

fetch "$URL" "$TMP/$ASSET" || die "download failed: $URL (the release may not exist yet)"

if [ "$VERIFY" = "1" ]; then
    fetch "$SUMS_URL" "$TMP/SHA256SUMS" \
        || die "could not download $SUMS_URL — re-run with --no-verify to skip the check, and read why you should not"
    # `sha256sum` writes `<hash>  <name>`, and its Windows build writes `<hash> *<name>`
    # for binary mode. Both forms are accepted; the name is compared as a whole field so a
    # lookalike asset cannot satisfy the check.
    expected=$(awk -v name="$ASSET" '{ name_field = $2; sub(/^\*/, "", name_field); if (name_field == name) { print $1; exit } }' "$TMP/SHA256SUMS")
    [ -n "$expected" ] || die "$SUMS_URL has no line for $ASSET"
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$TMP/$ASSET" | awk '{ print $1 }')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$TMP/$ASSET" | awk '{ print $1 }')
    elif command -v openssl >/dev/null 2>&1; then
        actual=$(openssl dgst -sha256 "$TMP/$ASSET" | sed 's/.*= //')
    else
        die "no sha256 tool (sha256sum, shasum or openssl) to verify the download — install one, or pass --no-verify"
    fi
    [ "$expected" = "$actual" ] || die "checksum mismatch for $ASSET: expected $expected, got $actual.
The download is corrupt or the release was replaced. Nothing was installed."
    note "  sha256 verified"
fi

mkdir -p "$TMP/unpack"
tar -xzf "$TMP/$ASSET" -C "$TMP/unpack" || die "could not unpack $ASSET"
# The archive holds one directory named for the release; the binary is inside it. Found
# rather than assumed, so a change to the archive's layout is a working installer or a
# failing one, and not a silent copy of the wrong file.
BIN=$(find "$TMP/unpack" -maxdepth 2 -type f -name kiomon -print -quit)
[ -n "$BIN" ] || die "$ASSET does not contain a kiomon binary"

mkdir -p "$DIR" || die "could not create $DIR"
if command -v install >/dev/null 2>&1; then
    install -m 755 "$BIN" "$DIR/kiomon"
else
    cp "$BIN" "$DIR/kiomon"
    chmod 755 "$DIR/kiomon"
fi

# The installed binary is asked what it is, which is the only proof that what landed is a
# program this machine can run. Skipped when the target was not the host's: there is
# nothing useful to ask a binary built for another platform.
if [ "$TARGET" = "$HOST" ]; then
    note "installed $("$DIR/kiomon" --version)"
else
    note "installed $DIR/kiomon (not executed: built for $TARGET, this machine is ${HOST:-unsupported})"
fi

case ":${PATH}:" in
    *":$DIR:"*) ;;
    *)
        note ""
        note "$DIR is not on your PATH. Add it:"
        note "  export PATH=\"$DIR:\$PATH\""
        note "  # and put that line in your shell's startup file to keep it"
        ;;
esac

note ""
note "Next: kiomon init && kiomon add --kind semantic --title \"first memory\" && kiomon search first"
