#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
elf="${1:-target/xtensa-esp32s3-espidf/debug/espal}"
bootloader="$(find target/xtensa-esp32s3-espidf -path '*/out/build/bootloader/bootloader.bin' -print -quit)"
test -n "$bootloader"
exec espflash flash "$elf" --port "${ESPFLASH_PORT:-/dev/ttyACM0}" \
  --before usb-reset --after hard-reset --skip-update-check \
  --flash-size 16mb --flash-mode dio --flash-freq 40mhz \
  --bootloader "$bootloader" --partition-table partitions.csv
