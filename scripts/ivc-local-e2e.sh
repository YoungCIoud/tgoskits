#!/usr/bin/env bash
set -euo pipefail

# ivc-local-e2e.sh
#
# 临时本地迭代工具：编译仓库内 vendored Linux IVC kernel driver、Linux
# 用户态程序、ArceOS guest，然后用 debugfs patch 本地 rootfs 镜像，并运行
# qemu-ivc 测例。
#
# 长期维护说明:
#   apps/linux/ivc/kernel_driver 是 vendored 源码，长期上游维护位置仍为
#   arceos-hypervisor/axvisor-tools。本脚本仅用于本地快速验证，不应替代
#   tgosimages 正式镜像构建。
#
# 用法:
#   scripts/ivc-local-e2e.sh [--fresh-image] [--no-run] [--no-arceos] [--fsck]
#
# 环境变量:
#   IVC_E2E_WORKSPACE         工作区根目录（默认: 本脚本所在仓库根目录）
#   IVC_E2E_KERNEL_DRIVER     kernel driver 源码目录
#                             （默认: apps/linux/ivc/kernel_driver）
#   IVC_E2E_KDIR              已配置/构建好的 guest Linux 内核源码树
#                             （默认 ~/workspace/tgosimages/build/qemu_linux）
#   IVC_E2E_ARCHIVE           rootfs tar.xz 归档
#                             （默认 tmp/axbuild/rootfs/rootfs-aarch64-alpine.img.tar.xz）
#   IVC_E2E_TIMEOUT           测例超时，例如 120s；为空则不加 timeout
#   IVC_E2E_SKIP_GUEST_BUILD=1 跳过 ArceOS guest .bin 构建（只 patch Linux 侧）
#   TGOS_IMAGE_LOCAL_STORAGE   兼容 axbuild 的本地镜像存储路径；设置后
#                              patch 和后续测例会使用同一份镜像缓存
#   IVC_E2E_PREBUILT_DIR       预编译产物目录（含 axvisor.ko / ivc-publish /
#                              ivc-subscribe）。设置后跳过本地 .ko 和 Linux
#                              用户态程序构建；给 CI 等没有 KDIR 的环境使用。
#
# 更新 CI 使用的 tracked 预编译产物:
#   scripts/ivc-local-e2e.sh --no-run --update-prebuilt

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="${IVC_E2E_WORKSPACE:-$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)}"
cd "$WORKSPACE"

KERNEL_DRIVER="${IVC_E2E_KERNEL_DRIVER:-$WORKSPACE/apps/linux/ivc/kernel_driver}"
KDIR="${IVC_E2E_KDIR:-/home/user/workspace/tgosimages/build/qemu_linux}"
ARCHIVE="${IVC_E2E_ARCHIVE:-$WORKSPACE/tmp/axbuild/rootfs/rootfs-aarch64-alpine.img.tar.xz}"
IMAGES_TOML_TEMPLATE="$WORKSPACE/tmp/axbuild/rootfs/images.toml"
LAST_SYNC_TEMPLATE="$WORKSPACE/tmp/axbuild/rootfs/.last_sync"

RUN_DIR="$WORKSPACE/tmp/ivc-local-e2e"
PREBUILT_DIR="${IVC_E2E_PREBUILT_DIR:-}"
LINUX_APPS="$RUN_DIR/linux-apps"
GUEST_BINS="$RUN_DIR/guest"
ARCEOS_BUILD_CONFIG="$RUN_DIR/arceos-build.toml"
LOCAL_STORAGE="${IVC_E2E_LOCAL_STORAGE:-${TGOS_IMAGE_LOCAL_STORAGE:-$WORKSPACE/tmp/axbuild-local/rootfs}}"
IMAGE_NAME="rootfs-aarch64-alpine.img"
IMAGE_DIR="$LOCAL_STORAGE/$IMAGE_NAME"
IMAGE="$IMAGE_DIR/$IMAGE_NAME"

FRESH_IMAGE=0
RUN_TEST=1
BUILD_ARCEOS=1
RUN_FSCK=0
UPDATE_PREBUILT=0
for arg in "$@"; do
    case "$arg" in
        --fresh-image) FRESH_IMAGE=1 ;;
        --no-run) RUN_TEST=0 ;;
        --no-arceos) BUILD_ARCEOS=0 ;;
        --fsck) RUN_FSCK=1 ;;
        --update-prebuilt) UPDATE_PREBUILT=1 ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done
if [ "$UPDATE_PREBUILT" = 1 ] && [ -n "$PREBUILT_DIR" ]; then
    die "--update-prebuilt cannot be used together with IVC_E2E_PREBUILT_DIR"
fi

log() { printf '\n\033[1;34m[ivc-local-e2e]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[ivc-local-e2e] error:\033[0m %s\n' "$*" >&2; exit 1; }

for tool in debugfs tar python3 cargo; do
    command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done
[ -f "$ARCHIVE" ] || die "rootfs archive not found: $ARCHIVE"
[ -f "$IMAGES_TOML_TEMPLATE" ] || die "images registry template not found: $IMAGES_TOML_TEMPLATE"

if [ -n "$PREBUILT_DIR" ]; then
    KERNEL_RELEASE="(prebuilt artifacts)"
else
    KERNEL_RELEASE="$(cat "$KDIR/include/config/kernel.release")"
fi
log "workspace        : $WORKSPACE"
log "guest kernel     : $KERNEL_RELEASE"
log "rootfs archive   : $ARCHIVE"

###############################################################################
# 1. 使用仓库内 vendored kernel driver 编译 v3/Message V1 模块
###############################################################################
if [ -n "$PREBUILT_DIR" ]; then
    MODULE="$PREBUILT_DIR/axvisor.ko"
    [ -f "$MODULE" ] || die "prebuilt module not found: $MODULE"
    log "using prebuilt Linux kernel module from $PREBUILT_DIR"
else
    command -v modinfo >/dev/null 2>&1 || die "missing required tool: modinfo"
    [ -f "$KERNEL_DRIVER/Makefile" ] \
        || die "vendored kernel driver not found: $KERNEL_DRIVER"
    [ -f "$KDIR/include/config/kernel.release" ] \
        || die "guest kernel source tree not ready: $KDIR"

    log "building Linux kernel module from $KERNEL_DRIVER"

    CROSS_COMPILE="aarch64-unknown-linux-musl-"
    if ! command -v "${CROSS_COMPILE}gcc" >/dev/null 2>&1; then
        CROSS_COMPILE="aarch64-linux-gnu-"
        command -v "${CROSS_COMPILE}gcc" >/dev/null 2>&1 \
            || die "no aarch64 kernel cross compiler found"
    fi

    make -C "$KERNEL_DRIVER" \
        CROSS_COMPILE="$CROSS_COMPILE" ARCH=arm64 KDIR="$KDIR" clean >/dev/null 2>&1 || true
    make -C "$KERNEL_DRIVER" \
        CROSS_COMPILE="$CROSS_COMPILE" ARCH=arm64 KDIR="$KDIR" -j"${IVC_E2E_JOBS:-4}"
    MODULE="$KERNEL_DRIVER/axvisor.ko"
    [ -f "$MODULE" ] || die "module build failed: axvisor.ko missing"
    MODULE_VERMAGIC="$(modinfo "$MODULE" | awk '/^vermagic:/{$1=""; print; exit}')"
    log "module vermagic   :${MODULE_VERMAGIC}"
fi

TRACKED_PREBUILT="$WORKSPACE/test-suit/axvisor/normal/qemu-ivc/prebuilt"

###############################################################################
# 2. 用当前仓库 apps/linux/ivc 编译 Linux 用户态 subscribe/publish
###############################################################################
if [ -n "$PREBUILT_DIR" ]; then
    LINUX_APPS="$PREBUILT_DIR"
    [ -f "$LINUX_APPS/ivc-publish" ] || die "prebuilt ivc-publish not found: $LINUX_APPS"
    [ -f "$LINUX_APPS/ivc-subscribe" ] || die "prebuilt ivc-subscribe not found: $LINUX_APPS"
    log "using prebuilt Linux user-space IVC apps from $PREBUILT_DIR"
else
    log "building Linux user-space IVC apps from apps/linux/ivc"
    rm -rf "$LINUX_APPS"
    AXVISOR_IVC_ARCH=aarch64 \
    AXVISOR_IVC_OUT_DIR="$LINUX_APPS" \
        "$WORKSPACE/apps/linux/ivc/build.sh"
    [ -f "$LINUX_APPS/ivc-subscribe" ] || die "ivc-subscribe build failed"
fi

###############################################################################
# 3. 用当前仓库源码编译 ArceOS guest publisher/subscriber
###############################################################################
if [ "$BUILD_ARCEOS" = 1 ]; then
    if [ "${IVC_E2E_SKIP_GUEST_BUILD:-0}" = 1 ]; then
        log "skipping ArceOS guest build (IVC_E2E_SKIP_GUEST_BUILD=1)"
    else
        log "building ArceOS guest apps from apps/arceos"
        cat > "$ARCEOS_BUILD_CONFIG" <<'EOF'
features = ["arceos", "ax-std/std-compat", "ax-std/tls"]
log = "Info"
max_cpu_num = 1
EOF
        rm -rf "$GUEST_BINS"
        mkdir -p "$GUEST_BINS"
        OBJCOPY="$(command -v llvm-objcopy || true)"
        if [ -z "$OBJCOPY" ]; then
            OBJCOPY="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/llvm-objcopy"
        fi
        for pkg in arceos-ivc-publisher arceos-ivc-subscriber; do
            cargo xtask arceos build \
                --arch aarch64 \
                --package "$pkg" \
                --config "$ARCEOS_BUILD_CONFIG"
            "$OBJCOPY" --binary-architecture=aarch64 -O binary \
                "$WORKSPACE/target/aarch64-unknown-linux-musl/release/$pkg" \
                "$GUEST_BINS/$pkg.bin"
        done
    fi
fi

###############################################################################
# 4. 准备本地 rootfs 镜像（干净归档解压 + debugfs patch）
###############################################################################
log "preparing local rootfs image"
mkdir -p "$LOCAL_STORAGE"
cp "$IMAGES_TOML_TEMPLATE" "$LOCAL_STORAGE/images.toml"
if [ -f "$LAST_SYNC_TEMPLATE" ]; then
    cp "$LAST_SYNC_TEMPLATE" "$LOCAL_STORAGE/.last_sync"
fi
ln -sfn "$ARCHIVE" "$LOCAL_STORAGE/$IMAGE_NAME.tar.xz"

EXPECTED_SHA="$(
    python3 - "$IMAGES_TOML_TEMPLATE" "$IMAGE_NAME" <<'PY'
import sys, tomllib
images = tomllib.load(open(sys.argv[1], 'rb'))['images']
name = sys.argv[2]
for image in images:
    if image['name'] == name:
        print(image['sha256'])
        break
else:
    raise SystemExit(f'image {name} not found')
PY
)"

if [ "$FRESH_IMAGE" = 1 ] || [ ! -f "$IMAGE" ]; then
    log "extracting fresh rootfs from archive"
    rm -rf "$IMAGE_DIR"
    mkdir -p "$IMAGE_DIR"
    tar -xJf "$ARCHIVE" -C "$IMAGE_DIR"
    printf '%s\n' "$EXPECTED_SHA" > "$IMAGE_DIR/.archive.sha256"
fi
[ -f "$IMAGE" ] || die "rootfs image is missing after prepare"

patch_image_file() {
    local host_path="$1"
    local guest_path="$2"
    local mode="$3"
    debugfs -w -R "rm $guest_path" "$IMAGE" >/dev/null 2>&1 || true
    debugfs -w -R "write $host_path $guest_path" "$IMAGE" >/dev/null \
        || die "failed to patch $guest_path"
    debugfs -w -R "sif $guest_path mode $mode" "$IMAGE" >/dev/null 2>&1 || true
}

if [ "$UPDATE_PREBUILT" = 1 ]; then
    log "updating tracked prebuilt artifacts under $TRACKED_PREBUILT"
    mkdir -p "$TRACKED_PREBUILT"
    cp "$MODULE" "$TRACKED_PREBUILT/axvisor.ko"
    cp "$LINUX_APPS/ivc-publish" "$TRACKED_PREBUILT/ivc-publish"
    cp "$LINUX_APPS/ivc-subscribe" "$TRACKED_PREBUILT/ivc-subscribe"
    chmod 644 "$TRACKED_PREBUILT/axvisor.ko"
    chmod 755 "$TRACKED_PREBUILT/ivc-publish" "$TRACKED_PREBUILT/ivc-subscribe"
fi

log "patching rootfs image"
patch_image_file "$MODULE" "/root/axvisor.ko" "0100644"
patch_image_file "$LINUX_APPS/ivc-subscribe" "/root/ivc-subscribe" "0100755"
patch_image_file "$LINUX_APPS/ivc-publish" "/root/ivc-publish" "0100755"
if [ "$BUILD_ARCEOS" = 1 ] && [ "${IVC_E2E_SKIP_GUEST_BUILD:-0}" != 1 ]; then
    patch_image_file "$GUEST_BINS/arceos-ivc-publisher.bin" \
        "/guest/arceos/arceos-ivc-publisher.bin" "0100755"
    patch_image_file "$GUEST_BINS/arceos-ivc-subscriber.bin" \
        "/guest/arceos/arceos-ivc-subscriber.bin" "0100755"
fi

if [ "$RUN_FSCK" = 1 ]; then
    log "running e2fsck -fn"
    e2fsck -fn "$IMAGE"
fi

###############################################################################
# 5. 运行 qemu-ivc
###############################################################################
if [ "$RUN_TEST" = 0 ]; then
    log "patched image ready (no run): $IMAGE"
    exit 0
fi

log "running qemu-ivc"
RUN_CMD=(
    cargo xtask axvisor test qemu
    --arch aarch64
    --test-group normal
    --test-case qemu-ivc
)
if [ -n "${IVC_E2E_TIMEOUT:-}" ]; then
    RUN_CMD=(timeout --kill-after=5s "$IVC_E2E_TIMEOUT" "${RUN_CMD[@]}")
fi
TGOS_IMAGE_LOCAL_STORAGE="$LOCAL_STORAGE" \
TMPDIR="$RUN_DIR" \
    "${RUN_CMD[@]}"
