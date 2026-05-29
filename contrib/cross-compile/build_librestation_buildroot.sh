#!/usr/bin/env sh
set -eu

PLUTOSDR_FW=${PLUTOSDR_FW:-/home/ubuntu/libresdr_timestamp_build/plutosdr-fw_0.38_libre}
HOST_DIR=${HOST_DIR:-$PLUTOSDR_FW/buildroot/output/host}
SYSROOT=${SYSROOT:-$HOST_DIR/arm-buildroot-linux-gnueabihf/sysroot}
TOOL_PREFIX=${TOOL_PREFIX:-$HOST_DIR/bin/arm-linux-gnueabihf}
TARGET=${TARGET:-arm-unknown-linux-gnueabihf}
OUT_DIR=${OUT_DIR:-/home/ubuntu/librestation_v2/artifacts/librestation}

if [ ! -x "${TOOL_PREFIX}-gcc" ]; then
    echo "missing Buildroot compiler: ${TOOL_PREFIX}-gcc" >&2
    exit 1
fi

if [ ! -e "$SYSROOT/lib/ld-linux-armhf.so.3" ]; then
    echo "sysroot does not look like the LibreSDR ARM hard-float rootfs: $SYSROOT" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"

export CARGO_BUILD_TARGET="$TARGET"
export CARGO_TARGET_ARM_UNKNOWN_LINUX_GNUEABIHF_LINKER="${TOOL_PREFIX}-gcc"
export CC_arm_unknown_linux_gnueabihf="${TOOL_PREFIX}-gcc"
export CXX_arm_unknown_linux_gnueabihf="${TOOL_PREFIX}-g++"
export AR_arm_unknown_linux_gnueabihf="${TOOL_PREFIX}-ar"
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$SYSROOT"
export PKG_CONFIG_PATH="$SYSROOT/usr/lib/pkgconfig:$SYSROOT/usr/share/pkgconfig"
export RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=cortex-a9 -C link-arg=--sysroot=$SYSROOT"

cargo build --release -p librestation-bs
cp "target/$TARGET/release/librestation-bs" "$OUT_DIR/librestation-bs"

echo "$OUT_DIR/librestation-bs"
