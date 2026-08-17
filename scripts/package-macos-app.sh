#!/bin/sh
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
    echo "error: macOS app packaging must run on macOS" >&2
    exit 1
fi

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
PROFILE=${1:-release}
TARGET_DIR=${CARGO_TARGET_DIR:-"$ROOT/target"}
case "$TARGET_DIR" in
    /*) ;;
    *) TARGET_DIR="$ROOT/$TARGET_DIR" ;;
esac

case "$PROFILE" in
    release)
        cargo build --manifest-path "$ROOT/Cargo.toml" --release \
            -p kimi-switch-gui -p kimi-subscription-router
        ;;
    debug)
        cargo build --manifest-path "$ROOT/Cargo.toml" \
            -p kimi-switch-gui -p kimi-subscription-router
        ;;
    *)
        echo "error: profile must be 'release' or 'debug'" >&2
        exit 1
        ;;
esac

BINARY="$TARGET_DIR/$PROFILE/kimi-switch"
ROUTER_BINARY="$TARGET_DIR/$PROFILE/kimi-subscription-router"
APP="$TARGET_DIR/$PROFILE/Kimi Subscription Router.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"

rm -rf "$APP"
mkdir -p "$MACOS"
cp "$BINARY" "$MACOS/kimi-switch"
cp "$ROUTER_BINARY" "$MACOS/kimi-subscription-router"
cp "$ROOT/packaging/macos/Info.plist" "$CONTENTS/Info.plist"
chmod 755 "$MACOS/kimi-switch"
chmod 755 "$MACOS/kimi-subscription-router"

PACKAGE_ID=$(cargo pkgid --manifest-path "$ROOT/Cargo.toml" -p kimi-switch-gui)
VERSION=${PACKAGE_ID##*@}
/usr/bin/plutil -replace CFBundleShortVersionString -string "$VERSION" "$CONTENTS/Info.plist"
/usr/bin/plutil -replace CFBundleVersion -string "$VERSION" "$CONTENTS/Info.plist"
/usr/bin/plutil -lint "$CONTENTS/Info.plist"

# 本地临时签名可避免 bundle 内容变更导致 macOS 拒绝启动。
/usr/bin/codesign --force --deep --sign - "$APP"

echo "$APP"
