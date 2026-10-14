#!/usr/bin/env bash
# Save a profile's container logs after a failed run, before stop.sh deletes
# the containers and their logs with them. Samba logs a client it cuts off
# (a credit or sequence violation, say) at level 0, and that line is often the
# only thing that tells "the server closed the socket" from "we tore it down".
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CRATE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
REPO_ROOT="$(cd "$CRATE_ROOT/../.." && pwd)"
PROFILE="${1:-internal}"

case "$PROFILE" in
    internal) COMPOSE_FILE="$SCRIPT_DIR/internal/docker-compose.yml" ;;
    consumer) COMPOSE_FILE="$CRATE_ROOT/src/testing/fixtures/consumer/docker-compose.yml" ;;
    *)
        echo "Usage: $0 {internal|consumer}"
        exit 1
        ;;
esac

OUT_DIR="$REPO_ROOT/target/docker-logs"
mkdir -p "$OUT_DIR"
OUT="$OUT_DIR/$PROFILE-$(date +%Y-%m-%d-%H%M%S).log"
docker compose -f "$COMPOSE_FILE" logs --timestamps --no-color > "$OUT" 2>&1
echo "[!] Container logs saved to $OUT"
