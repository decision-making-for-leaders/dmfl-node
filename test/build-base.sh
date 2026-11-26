#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=test/lib.sh
source "$SCRIPT_DIR/lib.sh"

sudo mkosi --image-version=1.0.0 --force build
sudo chown -R "$USER:$USER" mkosi.output
zstd -d mkosi.output/iota_1.0.0_x86-64.raw.zst -o ./disk-test.raw

{
  inspect_disk ./disk-test.raw "v1.0.0"
  echo "==== [v1.0.0] UKI ===="
  ls -la mkosi.output/
  ukify inspect mkosi.output/iota_1.0.0_x86-64.efi 2>&1 || true
} 2>&1 | tee /tmp/image-inspect-v100.txt

# Expand raw disk by 6G so systemd-repart can grow Slot A, create empty Slot B, and format /var
qemu-img resize -f raw ./disk-test.raw +6G
