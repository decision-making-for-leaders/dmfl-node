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

# Build v1.0.2 with a bad fleet firewall policy (blocks host egress) to test
# health-gated slot rollback without hacking /etc inside the guest.
cd - >/dev/null
BAD_POLICY=mkosi.extra/usr/lib/firewalld/policies/99-ci-bad-policy.xml
cleanup_bad_policy() {
  rm -f "$BAD_POLICY"
}
trap cleanup_bad_policy EXIT

cat > "$BAD_POLICY" <<'EOF'
<?xml version="1.0" encoding="utf-8"?>
<policy target="REJECT" priority="-10">
  <short>CI bad egress policy</short>
  <ingress-zone name="HOST"/>
  <egress-zone name="ANY"/>
</policy>
EOF

sudo rm -rf mkosi.output/*
sudo mkosi --image-version=1.0.2 --force build
sudo chown -R "$USER:$USER" mkosi.output
cleanup_bad_policy

{
  echo "==== [v1.0.2] build output ===="
  ls -la mkosi.output/
  ROOT102=$(find mkosi.output -maxdepth 1 -name 'iota_1.0.2_x86-64.root-*.raw.zst' ! -name '*verity*' | head -n 1)
  if [ -n "$ROOT102" ]; then
    zstd -dc "$ROOT102" > /tmp/root102.raw
    inspect_root /tmp/root102.raw "v1.0.2"
    rm -f /tmp/root102.raw
  else
    echo "(no non-verity root image found to inspect)"
  fi
  echo "==== [v1.0.2] UKI ===="
  ukify inspect mkosi.output/iota_1.0.2_x86-64.efi 2>&1 || true
} 2>&1 | tee /tmp/image-inspect-v102.txt

rm -rf /tmp/update-srv-v102
mkdir -p /tmp/update-srv-v102
cp mkosi.output/iota_1.0.2_x86-64.root-*.raw.zst /tmp/update-srv-v102/
cp mkosi.output/iota_1.0.2_x86-64.efi /tmp/update-srv-v102/
(
  cd /tmp/update-srv-v102
  sha256sum -- * > SHA256SUMS
  cat SHA256SUMS
)
