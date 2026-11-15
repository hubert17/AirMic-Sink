#!/usr/bin/env bash
set -euo pipefail

# AirMic-Sink Server + Blazor Client Docker Publish & Run Script (macOS)
# Builds AirMic.Server (which also hosts AirMic.Client Blazor WASM) and starts the container
# ready to be exposed via a manual cloudflared tunnel.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"

IMAGE_NAME="${IMAGE_NAME:-airmic-server:latest}"
CONTAINER_NAME="${CONTAINER_NAME:-airmic-server}"
PORT="${PORT:-8443}"
STREAM_SECRET="${STREAM_SECRET:-}"
MAX_PUBLIC_SESSIONS="${MAX_PUBLIC_SESSIONS:-10}"

if ! command -v docker >/dev/null 2>&1; then
    echo "Error: docker CLI is not installed or not in PATH." >&2
    exit 1
fi

if ! docker info >/dev/null 2>&1; then
    echo "Error: Docker daemon is not running. Start Docker Desktop / OrbStack / Colima and try again." >&2
    exit 1
fi

# Prompt for STREAM_SECRET if not supplied via environment variable
if [[ -z "${STREAM_SECRET}" ]]; then
    read -r -p "Enter STREAM_SECRET [leave blank to use appsettings.json PrivateMasterKeys]: " INPUT_SECRET
    STREAM_SECRET="${INPUT_SECRET}"
fi

echo "[1/3] Building Docker image '${IMAGE_NAME}' (AirMic.Server + AirMic.Client)..."
docker build \
    -t "${IMAGE_NAME}" \
    -f src/AirMic.Server/Dockerfile \
    .

echo "[2/3] Replacing any existing '${CONTAINER_NAME}' container..."
if docker ps -a --format '{{.Names}}' | grep -qx "${CONTAINER_NAME}"; then
    docker rm -f "${CONTAINER_NAME}" >/dev/null
fi

echo "[3/3] Starting '${CONTAINER_NAME}' on http://localhost:${PORT}..."
DOCKER_ENV_ARGS=(
    -e "PORT=${PORT}"
    -e "MAX_PUBLIC_SESSIONS=${MAX_PUBLIC_SESSIONS}"
)
if [[ -n "${STREAM_SECRET}" ]]; then
    DOCKER_ENV_ARGS+=(-e "STREAM_SECRET=${STREAM_SECRET}")
fi

docker run -d \
    --name "${CONTAINER_NAME}" \
    --restart unless-stopped \
    -p "${PORT}:${PORT}" \
    "${DOCKER_ENV_ARGS[@]}" \
    "${IMAGE_NAME}" >/dev/null

echo ""
echo "AirMic Server + Client container is running."
echo "  Container : ${CONTAINER_NAME}"
echo "  Client UI : http://localhost:${PORT}"
echo "  WebSocket : ws://localhost:${PORT}/ws"
echo "  Logs      : docker logs -f ${CONTAINER_NAME}"
echo ""
echo "Cloudflare Tunnel next step:"
echo "  Point your cloudflared tunnel service URL to: http://localhost:${PORT}"