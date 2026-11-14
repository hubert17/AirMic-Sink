#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${CRATE_DIR}/../.." && pwd)"

TARGET_TRIPLE="aarch64-apple-darwin"
APP_NAME="AirMicReceiver"
BUNDLE_DIR="${CRATE_DIR}/target/release/${APP_NAME}.app"
CONTENTS_DIR="${BUNDLE_DIR}/Contents"
MACOS_DIR="${CONTENTS_DIR}/MacOS"
RESOURCES_DIR="${CONTENTS_DIR}/Resources"

echo "[1/5] Building ${APP_NAME} (${TARGET_TRIPLE}, release)..."
cd "${CRATE_DIR}"
LIBOPUS_STATIC=1 cargo build --release --target "${TARGET_TRIPLE}"

BIN_PATH="${CRATE_DIR}/target/${TARGET_TRIPLE}/release/${APP_NAME}"
if [[ ! -f "${BIN_PATH}" ]]; then
    echo "Error: Expected compiled binary at ${BIN_PATH}" >&2
    exit 1
fi

echo "[2/5] Assembling macOS .app bundle at ${BUNDLE_DIR}..."
rm -rf "${BUNDLE_DIR}"
mkdir -p "${MACOS_DIR}" "${RESOURCES_DIR}"

cp "${BIN_PATH}" "${MACOS_DIR}/${APP_NAME}"
chmod +x "${MACOS_DIR}/${APP_NAME}"
cp "${CRATE_DIR}/resources/Info.plist" "${CONTENTS_DIR}/Info.plist"
printf "APPL????" > "${CONTENTS_DIR}/PkgInfo"

ICON_SRC="${REPO_ROOT}/src/AirMic.Client/wwwroot/icon-512.png"
if [[ -f "${ICON_SRC}" ]] && command -v sips >/dev/null 2>&1 && command -v iconutil >/dev/null 2>&1; then
    echo "[3/5] Generating Retina AppIcon.icns from ${ICON_SRC}..."
    TMP_ICON_ROOT="$(mktemp -d)"
    ICONSET_DIR="${TMP_ICON_ROOT}/AppIcon.iconset"
    mkdir -p "${ICONSET_DIR}"

    for size in 16 32 128 256 512; do
        sips -z "${size}" "${size}" "${ICON_SRC}" --out "${ICONSET_DIR}/icon_${size}x${size}.png" >/dev/null 2>&1
        double=$((size * 2))
        sips -z "${double}" "${double}" "${ICON_SRC}" --out "${ICONSET_DIR}/icon_${size}x${size}@2x.png" >/dev/null 2>&1
    done

    iconutil -c icns "${ICONSET_DIR}" -o "${RESOURCES_DIR}/AppIcon.icns"
    rm -rf "${TMP_ICON_ROOT}"
else
    echo "[3/5] Skipping .icns generation (icon source or iconutil unavailable)."
fi

echo "[4/5] Verifying Mach-O architecture and self-contained dynamic library links..."
lipo -archs "${MACOS_DIR}/${APP_NAME}"
if otool -L "${MACOS_DIR}/${APP_NAME}" | grep -q "/opt/homebrew"; then
    echo "Error: Binary links against /opt/homebrew dylib instead of static libopus!" >&2
    otool -L "${MACOS_DIR}/${APP_NAME}" >&2
    exit 1
fi

echo "[5/5] Applying ad-hoc codesign to ${BUNDLE_DIR}..."
xattr -cr "${BUNDLE_DIR}" 2>/dev/null || true
codesign --force --deep --sign - "${BUNDLE_DIR}"
codesign --verify --deep --strict "${BUNDLE_DIR}"

echo "Built and signed: ${BUNDLE_DIR}"
