#!/bin/sh
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
    echo "error: macOS installation must run on macOS" >&2
    exit 1
fi

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TARGET_DIR=${CARGO_TARGET_DIR:-"$ROOT/target"}
case "$TARGET_DIR" in
    /*) ;;
    *) TARGET_DIR="$ROOT/$TARGET_DIR" ;;
esac

INSTALL_DIR=${KIMI_ROUTER_INSTALL_DIR:-"$HOME/Applications"}
APP_NAME="Kimi Subscription Router.app"
SOURCE_APP="$TARGET_DIR/release/$APP_NAME"
DESTINATION="$INSTALL_DIR/$APP_NAME"
BACKUP_ROOT="$INSTALL_DIR/.kimi-subscription-router-backups"
STAMP=$(date +%Y%m%d-%H%M%S)
BACKUP="$BACKUP_ROOT/$APP_NAME.$STAMP"
BACKUP_INDEX=0
while [ -e "$BACKUP" ]; do
    BACKUP_INDEX=$((BACKUP_INDEX + 1))
    BACKUP="$BACKUP_ROOT/$APP_NAME.$STAMP-$BACKUP_INDEX"
done
STAGED="$INSTALL_DIR/.Kimi Subscription Router.installing.$$"
MOVED_OLD=0

cleanup() {
    rm -rf "$STAGED"
}

rollback() {
    cleanup
    if [ "$MOVED_OLD" -eq 1 ] && [ ! -e "$DESTINATION" ] && [ -e "$BACKUP" ]; then
        mv "$BACKUP" "$DESTINATION"
        echo "restored previous application after installation failure" >&2
    fi
}

trap rollback HUP INT TERM EXIT

"$ROOT/scripts/package-macos-app.sh" release
mkdir -p "$INSTALL_DIR" "$BACKUP_ROOT"
/usr/bin/ditto "$SOURCE_APP" "$STAGED"

if [ -n "${KIMI_ROUTER_SIGNING_IDENTITY:-}" ]; then
    /usr/bin/codesign --force --deep --options runtime \
        --sign "$KIMI_ROUTER_SIGNING_IDENTITY" "$STAGED"
fi

/usr/bin/plutil -lint "$STAGED/Contents/Info.plist" >/dev/null
/usr/bin/codesign --verify --deep --strict --verbose=2 "$STAGED"
test -x "$STAGED/Contents/MacOS/Kimi Subscription Router"
test -x "$STAGED/Contents/MacOS/kimi-subscription-router"

if [ -e "$DESTINATION" ]; then
    mv "$DESTINATION" "$BACKUP"
    MOVED_OLD=1
fi
mv "$STAGED" "$DESTINATION"

trap - HUP INT TERM EXIT
cleanup

echo "installed: $DESTINATION"
if [ "$MOVED_OLD" -eq 1 ]; then
    echo "backup: $BACKUP"
fi

if [ "${KIMI_ROUTER_NO_LAUNCH:-0}" != "1" ]; then
    open "$DESTINATION"
fi
