#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tests/lib.sh
source "$SCRIPT_DIR/lib.sh"

sudo rm -rf mkosi.output/*
sudo mkosi --image-version=1.0.1 --force build
sudo chown -R "$USER:$USER" mkosi.output

{
  echo "==== [v1.0.1] build output ===="
  ls -la mkosi.output/
  ROOT101=$(find mkosi.output -maxdepth 1 -name 'iota_1.0.1_x86-64.root-*.raw.zst' ! -name '*verity*' | head -n 1)
  if [ -n "$ROOT101" ]; then
    zstd -dc "$ROOT101" > /tmp/root101.raw
    inspect_root /tmp/root101.raw "v1.0.1"
    rm -f /tmp/root101.raw
  else
    echo "(no non-verity root image found to inspect)"
  fi
  echo "==== [v1.0.1] UKI ===="
  ukify inspect mkosi.output/iota_1.0.1_x86-64.efi 2>&1 || true
} 2>&1 | tee /tmp/image-inspect-v101.txt

mkdir -p /tmp/update-srv
cp mkosi.output/iota_1.0.1_x86-64.root-*.raw.zst /tmp/update-srv/
cp mkosi.output/iota_1.0.1_x86-64.efi /tmp/update-srv/

cd /tmp/update-srv
sha256sum -- * > SHA256SUMS
cat SHA256SUMS
setsid nohup python3 -m http.server 8000 --bind 0.0.0.0 > /tmp/http-server.log 2>&1 &
HTTP_PID=$!
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "HTTP_PID=$HTTP_PID" >> "$GITHUB_ENV"
fi
