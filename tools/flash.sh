#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
elf="${1:-target/xtensa-esp32s3-espidf/debug/espal}"
# Pack and flash the selected offline wake model into unused flash beyond the
# factory app. NVS/PHY offsets and data remain untouched.
build_output=""
while IFS= read -r candidate; do
  if test -f "$candidate/sdkconfig" && test -f "$candidate/build/bootloader/bootloader.bin"; then
    build_output="$candidate"
    break
  fi
done < <(find target/xtensa-esp32s3-espidf/debug/build -type d -name out)
test -n "$build_output"
bootloader="$build_output/build/bootloader/bootloader.bin"
sr_component="$build_output/managed_components/espressif__esp-sr"
test -d "$sr_component"
python3 "$sr_component/model/movemodel.py" -d1 "$build_output/sdkconfig" -d2 "$sr_component" -d3 "$build_output/build"
sr_image="$build_output/build/srmodels/srmodels.bin"
test -s "$sr_image"
test "$(wc -c < "$sr_image")" -le 4194304
espflash write-bin 0x310000 "$sr_image" --port "${ESPFLASH_PORT:-/dev/ttyACM0}" \
  --before usb-reset --after hard-reset --skip-update-check
exec espflash flash "$elf" --port "${ESPFLASH_PORT:-/dev/ttyACM0}" \
  --before usb-reset --after hard-reset --skip-update-check \
  --flash-size 16mb --flash-mode dio --flash-freq 40mhz \
  --bootloader "$bootloader" --partition-table partitions.csv
