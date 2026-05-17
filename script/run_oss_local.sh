#!/bin/bash
# Build and run Warp OSS with DeepSeek AI proxy + skip login
set -e

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

export PATH="$HOME/.cargo/bin:$PATH"

echo "=== Building Warp OSS with DeepSeek proxy ==="
cd app && cargo bundle --bin warp-oss --features "gui,skip_login" "$@"

echo "=== Setting up bundle ==="
cd "$REPO_ROOT"
WARP_APP_PATH="target/debug/bundle/osx/WarpOss.app"
WARP_BIN_NAME="warp-oss"

install_name_tool -add_rpath "@executable_path/../Frameworks" "$WARP_APP_PATH/Contents/MacOS/$WARP_BIN_NAME" 2>/dev/null || true

export WARP_SCHEME_NAME="warp" WARP_PLIST_PATH="$WARP_APP_PATH/Contents/Info.plist"
bash script/update_plist

SKIP_SETTINGS_SCHEMA=1 NO_LICENSES=1 bash script/prepare_bundled_resources "$WARP_APP_PATH/Contents/Resources" "oss" > /dev/null 2>&1

echo "=== Customizing appearance ==="
plutil -replace CFBundleDisplayName -string "Warp" "$WARP_APP_PATH/Contents/Info.plist" 2>/dev/null || true
plutil -replace CFBundleName -string "Warp" "$WARP_APP_PATH/Contents/Info.plist" 2>/dev/null || true

echo "=== Signing ==="
codesign --force --deep --options runtime --sign "-" "$WARP_APP_PATH" --entitlements script/Debug-Entitlements.plist 2>/dev/null

echo "=== Launching Warp + DeepSeek ==="
echo "Config: ~/.warp/setting.json"
echo ""
"./$WARP_APP_PATH/Contents/MacOS/$WARP_BIN_NAME"
