#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="${IVC_E2E_WORKSPACE:-$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)}"
KDIR="${IVC_E2E_KDIR:-$WORKSPACE/tmp/ivc-linux-kernel}"
LINUX_REF="${IVC_E2E_LINUX_REF:-74fe02ce122a6103f207d29fafc8b3a53de6abaf}"
EXPECTED_LINUX_REF="74fe02ce122a6103f207d29fafc8b3a53de6abaf"
EXPECTED_ARCHIVE_SHA256="12f4614ceb3126987fd3abbd989a442c80f2baf521b6efbd8e5aa0fc15e892f6"
EXPECTED_KERNEL_RELEASE="7.1.0-rc2-g74fe02ce122a-dirty"
DOWNLOAD_DIR="${IVC_E2E_DOWNLOAD_DIR:-$WORKSPACE/tmp/ivc-downloads}"
ARCHIVE="$DOWNLOAD_DIR/linux-$LINUX_REF.tar.gz"
SOURCE_URL="https://github.com/torvalds/linux/archive/$LINUX_REF.tar.gz"
SOURCE_MARKER="$KDIR/.ivc-e2e-linux-source"
JOBS="${IVC_E2E_JOBS:-4}"

log() { printf '\n\033[1;34m[ivc-prepare-linux-kernel]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[ivc-prepare-linux-kernel] error:\033[0m %s\n' "$*" >&2; exit 1; }

if [ "$LINUX_REF" != "$EXPECTED_LINUX_REF" ]; then
    die "IVC_E2E_LINUX_REF must match the qemu guest kernel ref $EXPECTED_LINUX_REF"
fi

kernel_tree_is_ready() {
    [ -f "$KDIR/include/config/kernel.release" ] \
        && [ -f "$KDIR/include/generated/autoconf.h" ] \
        && [ -f "$KDIR/Module.symvers" ] \
        && [ -x "$KDIR/scripts/mod/modpost" ] \
        && [ "$(cat "$KDIR/include/config/kernel.release")" = "$EXPECTED_KERNEL_RELEASE" ] \
        && {
            case "$KDIR" in
                "$WORKSPACE"/tmp/*)
                    [ -f "$SOURCE_MARKER" ] \
                        && [ "$(cat "$SOURCE_MARKER")" = "$LINUX_REF $EXPECTED_ARCHIVE_SHA256" ]
                    ;;
                *) true ;;
            esac
        }
}

if kernel_tree_is_ready; then
    log "using prepared guest kernel tree: $KDIR"
    printf 'IVC_E2E_KDIR=%s\n' "$KDIR"
    exit 0
fi

for tool in bc bison curl flex make pkg-config sha256sum tar; do
    command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done
pkg-config --exists libcrypto \
    || die "missing libcrypto development files required by the Linux build"

CROSS_COMPILE="aarch64-unknown-linux-musl-"
if ! command -v "${CROSS_COMPILE}gcc" >/dev/null 2>&1; then
    CROSS_COMPILE="aarch64-linux-gnu-"
    command -v "${CROSS_COMPILE}gcc" >/dev/null 2>&1 \
        || die "no aarch64 kernel cross compiler found"
fi

case "$KDIR" in
    "$WORKSPACE"/tmp/*) ;;
    *) die "refusing to replace incompatible kernel tree outside $WORKSPACE/tmp: $KDIR" ;;
esac

log "downloading Linux $LINUX_REF"
mkdir -p "$DOWNLOAD_DIR"
if [ ! -f "$ARCHIVE" ]; then
    archive_tmp="$ARCHIVE.tmp"
    rm -f "$archive_tmp"
    curl --fail --location --retry 3 --show-error --silent \
        --output "$archive_tmp" "$SOURCE_URL"
    mv "$archive_tmp" "$ARCHIVE"
fi
if ! printf '%s  %s\n' "$EXPECTED_ARCHIVE_SHA256" "$ARCHIVE" | sha256sum --check --status; then
    rm -f "$ARCHIVE"
    die "Linux source archive checksum mismatch"
fi

log "extracting guest kernel source"
rm -rf "$KDIR"
mkdir -p "$KDIR"
tar -xzf "$ARCHIVE" --strip-components=1 -C "$KDIR"

log "building guest kernel metadata for external modules"
make -C "$KDIR" ARCH=arm64 CROSS_COMPILE="$CROSS_COMPILE" defconfig
"$KDIR/scripts/config" --file "$KDIR/.config" \
    --set-str LOCALVERSION "-g74fe02ce122a-dirty" \
    --disable LOCALVERSION_AUTO
make -C "$KDIR" ARCH=arm64 CROSS_COMPILE="$CROSS_COMPILE" olddefconfig
# A complete vmlinux link generates Module.symvers. modules_prepare alone does
# not provide the exported-symbol metadata needed for strict external-module
# modpost validation.
make -C "$KDIR" ARCH=arm64 CROSS_COMPILE="$CROSS_COMPILE" -j"$JOBS" vmlinux
cp "$KDIR/vmlinux.symvers" "$KDIR/Module.symvers"
printf '%s %s\n' "$LINUX_REF" "$EXPECTED_ARCHIVE_SHA256" > "$SOURCE_MARKER"

kernel_tree_is_ready \
    || die "prepared kernel release does not match $EXPECTED_KERNEL_RELEASE"

printf 'IVC_E2E_KDIR=%s\n' "$KDIR"
