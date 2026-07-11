#!/usr/bin/env bash
# build-image.sh — build a curated rootfs image into an ext4 disk for Firecracker.
#
#   sudo bash deploy/build-image.sh <name> [size]
#
# Reads deploy/images/<name>/Dockerfile (+ sandbox-init), bakes in the Linux
# guest agent, exports the container filesystem, and writes
# /var/lib/workdir/images/<name>/rootfs.ext4.
#
#   name   image key: base | node-python | browser | ...
#   size   ext4 size (default 4G)
set -euo pipefail

NAME="${1:?usage: build-image.sh <name> [size]}"
SIZE="${2:-4G}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA_DIR=/var/lib/workdir
SRC="$REPO_ROOT/deploy/images/$NAME"
OUT="$DATA_DIR/images/$NAME/rootfs.ext4"

[ -f "$SRC/Dockerfile" ] || { echo "no Dockerfile at $SRC" >&2; exit 1; }
command -v docker >/dev/null || { echo "docker required" >&2; exit 1; }

# The guest agent must be built for Linux first (cargo build --release).
AGENT="$REPO_ROOT/target/release/sandbox-guest-agent"
[ -x "$AGENT" ] || AGENT="$DATA_DIR/sandbox-guest-agent"
[ -x "$AGENT" ] || { echo "sandbox-guest-agent not found — run: cargo build --release -p guest-agent" >&2; exit 1; }

echo "==> building image '$NAME' ($SIZE)"
build=$(mktemp -d)
cp "$SRC"/* "$build"/
cp "$AGENT" "$build/sandbox-guest-agent"
docker build --network=host -t "workdir-$NAME" "$build" >/dev/null

rootfs="$build/rootfs"
mkdir -p "$rootfs"
cid=$(docker create "workdir-$NAME")
docker export "$cid" | tar -C "$rootfs" -xf -
docker rm "$cid" >/dev/null

install -d -o workdir -g workdir "$DATA_DIR/images/$NAME"
rm -f "$OUT"
truncate -s "$SIZE" "$OUT"
mkfs.ext4 -F -q -d "$rootfs" -L "workdir-$NAME" "$OUT"
chown -R workdir:workdir "$DATA_DIR/images/$NAME"
rm -rf "$build"
echo "==> wrote $OUT ($(du -h "$OUT" | cut -f1) provisioned, $(du -h --apparent-size "$OUT" | cut -f1) virtual)"
