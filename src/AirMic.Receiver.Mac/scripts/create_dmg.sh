#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

APP_BUNDLE="${CRATE_DIR}/target/release/AirMicReceiver.app"
DMG_PATH="${CRATE_DIR}/target/release/AirMic.Receiver-macos-arm64.dmg"
VOL_NAME="AirMic Receiver"

if [[ ! -d "${APP_BUNDLE}" ]]; then
    echo "App bundle not found; running build_app.sh first..."
    "${SCRIPT_DIR}/build_app.sh"
fi

STAGING_DIR="$(mktemp -d)"
trap 'rm -rf "${STAGING_DIR}"' EXIT

echo "[1/2] Preparing DMG staging folder with /Applications symlink..."
cp -R "${APP_BUNDLE}" "${STAGING_DIR}/AirMicReceiver.app"
ln -s /Applications "${STAGING_DIR}/Applications"

echo "[2/2] Creating compressed Apple Silicon disk image ${DMG_PATH}..."
rm -f "${DMG_PATH}"
hdiutil create -volname "${VOL_NAME}" -srcfolder "${STAGING_DIR}" -ov -format UDZO "${DMG_PATH}" >/dev/null

codesign --force --sign - "${DMG_PATH}" 2>/dev/null || true

echo "Created DMG: ${DMG_PATH}"
ls -lh "${DMG_PATH}"
