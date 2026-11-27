#!/usr/bin/env bash
set -euo pipefail

if mkosi summary --json | jq -e '.Images[] | select(.Image == "main") | (.Profiles // []) | index("ssh-var-luks") != null' >/dev/null; then
  SSH_VAR_LUKS=true
else
  SSH_VAR_LUKS=false
fi
LUKS_PASSPHRASE=ci-provision-passphrase

if [ -n "${GITHUB_ENV:-}" ]; then
  {
    echo "SSH_VAR_LUKS=$SSH_VAR_LUKS"
    echo "LUKS_PASSPHRASE=$LUKS_PASSPHRASE"
  } >> "$GITHUB_ENV"
fi

# Generate SSH key on the host only; do NOT bake keys or sshd overrides into mkosi.extra/.
ssh-keygen -t ed25519 -f /tmp/ci_id_ed25519 -N ""

# Point sysupdate [Source] transfer definitions at the QEMU host HTTP server
sed -i 's|^Path=https://.*|Path=http://10.0.2.2:8000/|' mkosi.extra/usr/lib/sysupdate.d/*.transfer

# Enable serial console logging for headless QEMU CI builds only
mkdir -p mkosi.conf.d
printf '[Content]\nKernelCommandLine=console=ttyS0,115200n8\n' > mkosi.conf.d/99-ci.conf
