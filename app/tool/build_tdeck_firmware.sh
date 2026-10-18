#!/usr/bin/env bash
# Builds ../tdeck (T-Deck profile) with ESP-IDF and installs the images into the app's T-Deck
# firmware bundle (default assets/firmware/tdeck), writing manifest.json with offsets and MD5 sums.
#
#   tool/build_tdeck_firmware.sh [IDF_PATH] [OUT_DIR]
# IDF_PATH defaults to $IDF_PATH (the bundle is built with ESP-IDF 6.1).
set -euo pipefail
APP=$(cd "$(dirname "$0")/.." && pwd)
FW=$(cd "$APP/../tdeck" && pwd)
IDF=${1:-${IDF_PATH:?set IDF_PATH or pass the ESP-IDF directory}}
OUT=${2:-$APP/assets/firmware/tdeck}
(
  cd "$FW"
  # shellcheck disable=SC1091
  . "$IDF/export.sh" > /dev/null
  idf.py build > build/bundle.log 2>&1 || { tail -30 build/bundle.log; exit 1; }
)
IDF_VER=$(cd "$IDF" && git describe --tags 2>/dev/null || cat "$IDF/version.txt")
COMMIT=$(git -C "$FW" rev-parse --short HEAD)$(git -C "$FW" diff --quiet -- . || echo "-dirty")
mkdir -p "$OUT"
B=$FW/build
cp "$B/bootloader/bootloader.bin" "$B/partition_table/partition-table.bin" "$B/tdeck_datv.bin" "$OUT/"
INFO=$(grep -o 'TDECKDATV [0-9]*' "$FW/main/main.c" | head -1)
VER=${INFO#TDECKDATV }
read -r MODE FREQ SIZE < <(awk 'NR == 1 { print $2, $4, $6 }' "$B/flash_args")
md5() { md5sum "$OUT/$1" | cut -d' ' -f1; }
cat > "$OUT/manifest.json" <<JSON
{
  "chip": "esp32s3",
  "info": "$INFO",
  "version": $VER,
  "source_commit": "$COMMIT",
  "idf": "$IDF_VER",
  "flash_mode": "$MODE",
  "flash_freq": "$FREQ",
  "flash_size": "$SIZE",
  "images": [
    {"file": "bootloader.bin", "offset": 0, "md5": "$(md5 bootloader.bin)"},
    {"file": "partition-table.bin", "offset": 32768, "md5": "$(md5 partition-table.bin)"},
    {"file": "tdeck_datv.bin", "offset": 65536, "md5": "$(md5 tdeck_datv.bin)"}
  ]
}
JSON
echo "T-Deck firmware $INFO from $COMMIT built with ESP-IDF $IDF_VER into $OUT"
