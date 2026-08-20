#!/bin/sh
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
    echo "error: macOS release packaging must run on macOS" >&2
    exit 1
fi

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ARCH=${1:-$(uname -m)}
VERSION=${2:-}
TARGET_DIR=${CARGO_TARGET_DIR:-"$ROOT/target"}
case "$TARGET_DIR" in
    /*) ;;
    *) TARGET_DIR="$ROOT/$TARGET_DIR" ;;
esac

if [ -z "$VERSION" ]; then
    PACKAGE_ID=$(cargo pkgid --manifest-path "$ROOT/Cargo.toml" -p kimi-subscription-router-gui)
    VERSION=${PACKAGE_ID##*@}
fi

case "$ARCH" in
    arm64|x86_64) ;;
    *)
        echo "error: unsupported macOS architecture: $ARCH" >&2
        exit 1
        ;;
esac

"$ROOT/scripts/package-macos-app.sh" release

APP="$TARGET_DIR/release/Kimi Subscription Router.app"
CLI="$TARGET_DIR/release/Kimi Subscription Router CLI"
ROUTER="$TARGET_DIR/release/kimi-subscription-router"
DIST="$TARGET_DIR/dist/macos-$ARCH"
PAYLOAD="$DIST/payload"
DMG_ROOT="$DIST/dmg-root"
ZIP="$DIST/Kimi-Subscription-Router-$VERSION-macOS-$ARCH.zip"
DMG="$DIST/Kimi-Subscription-Router-$VERSION-macOS-$ARCH.dmg"

rm -rf "$DIST"
mkdir -p "$PAYLOAD" "$DMG_ROOT"
/usr/bin/ditto "$APP" "$PAYLOAD/Kimi Subscription Router.app"
cp "$CLI" "$PAYLOAD/Kimi Subscription Router CLI"
cp "$ROUTER" "$PAYLOAD/kimi-subscription-router"
chmod 755 "$PAYLOAD/Kimi Subscription Router CLI" "$PAYLOAD/kimi-subscription-router"

(cd "$PAYLOAD" && /usr/bin/ditto -c -k --sequesterRsrc --keepParent \
    "Kimi Subscription Router.app" "$ZIP")

/usr/bin/ditto "$APP" "$DMG_ROOT/Kimi Subscription Router.app"
ln -s /Applications "$DMG_ROOT/Applications"
/usr/bin/hdiutil create \
    -volname "Kimi Subscription Router" \
    -srcfolder "$DMG_ROOT" \
    -ov \
    -format UDZO \
    "$DMG"

/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP"
VERIFY_ATTEMPT=1
while ! /usr/bin/hdiutil verify "$DMG"; do
    if [ "$VERIFY_ATTEMPT" -ge 5 ]; then
        echo "error: DMG verification failed after $VERIFY_ATTEMPT attempts" >&2
        exit 1
    fi
    VERIFY_ATTEMPT=$((VERIFY_ATTEMPT + 1))
    sleep 2
done
/usr/bin/shasum -a 256 "$ZIP" "$DMG" |
    sed "s#  .*/#  #" > "$DIST/SHA256SUMS-macOS-$ARCH.txt"

echo "$ZIP"
echo "$DMG"
