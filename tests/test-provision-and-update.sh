#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tests/lib.sh
source "$SCRIPT_DIR/lib.sh"

if [ -z "${SSH_VAR_LUKS:-}" ]; then
  if mkosi summary --json | jq -e '.Images[] | select(.Image == "main") | (.Profiles // []) | index("ssh-var-luks") != null' >/dev/null; then
    SSH_VAR_LUKS=true
  else
    SSH_VAR_LUKS=false
  fi
fi
LUKS_PASSPHRASE=${LUKS_PASSPHRASE:-ci-provision-passphrase}

EXPECTED_MACHINE_ID=$(tr -d '-' < /proc/sys/kernel/random/uuid)
EXPECTED_HOSTNAME="iota-provisioned"
EXPECTED_WG_ADDR="10.100.0.42/24"

PASS_FILE=$(mktemp /tmp/ci-luks-pass.XXXXXX)
chmod 0600 "$PASS_FILE"
printf '%s' "$LUKS_PASSPHRASE" > "$PASS_FILE"

QEMU_PID=""
cleanup() {
  rm -f "$PASS_FILE"
  if [ -n "$QEMU_PID" ]; then
    kill "$QEMU_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

PROVISION_ARGS=(
  --target ./disk-test.raw
  --ssh-key /tmp/ci_id_ed25519.pub
  --wg-address "$EXPECTED_WG_ADDR"
  --hostname "$EXPECTED_HOSTNAME"
  --machine-id "$EXPECTED_MACHINE_ID"
)
if [ "$SSH_VAR_LUKS" = "true" ]; then
  PROVISION_ARGS+=(--luks-passphrase-file "$PASS_FILE")
else
  PROVISION_ARGS+=(--no-luks)
fi

PROVISION_BIN="$SCRIPT_DIR/../target/release/iota-provision"
if [ ! -x "$PROVISION_BIN" ]; then
  cargo build --release --manifest-path "$SCRIPT_DIR/../Cargo.toml" -p iota-provision
fi

PROVISION_OUT=$(sudo "$PROVISION_BIN" "${PROVISION_ARGS[@]}")
printf '%s\n' "$PROVISION_OUT"
rm -f "$PASS_FILE"

EXPECTED_WG_PUBKEY=$(printf '%s\n' "$PROVISION_OUT" | sed -n 's/^WireGuard public key: //p' | tr -d '[:space:]')
test -n "$EXPECTED_WG_PUBKEY"

verify_bless_boot() {
  ssh_guest '
    set -euo pipefail
    ESP_DIR=$(bootctl --print-esp-path)
    BOOTCOUNT_VAR="/sys/firmware/efi/efivars/LoaderBootCountPath-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
    test -f "$BOOTCOUNT_VAR"
    tr -d "\0" < "$BOOTCOUNT_VAR" | grep -qaF "iota_1.0.1_x86-64+2-1.efi"
    BLESS_STATUS=$(/usr/lib/systemd/systemd-bless-boot status || true)
    if [ "$BLESS_STATUS" != "good" ]; then
      test -f "$ESP_DIR/EFI/Linux/iota_1.0.1_x86-64+2-1.efi"
    fi
    systemctl start systemd-bless-boot.service
    /usr/lib/systemd/systemd-bless-boot status | grep -qx good
    ls -la "$ESP_DIR/EFI/Linux/"
    test -f "$ESP_DIR/EFI/Linux/iota_1.0.1_x86-64.efi"
    ! find "$ESP_DIR/EFI/Linux/" -maxdepth 1 -name "iota_1.0.1_x86-64+*.efi" | grep -q .
  '
}

# Copy writable OVMF_VARS so UEFI boot entries persist across reboots
cp /usr/share/OVMF/OVMF_VARS_4M.fd ./ovmf_vars.fd

# Boot 1: v1.0.0 with offline-provisioned LUKS, SSH keys, WireGuard credential, hostname, and machine-id
start_qemu /tmp/serial-provision-boot1.log /tmp/serial-provision-boot1.sock -no-reboot

if [ "$SSH_VAR_LUKS" = "true" ]; then
  echo "Unlocking /var via initrd SSH for provisioned v1.0.0..."
  unlock_var_ssh 45 /tmp/serial-provision-boot1.log "$LUKS_PASSPHRASE"
fi

echo "Waiting for provisioned v1.0.0 guest SSH..."
if ! wait_ssh 60 /tmp/serial-provision-boot1.log; then
  capture_serial_diagnostics /tmp/serial-provision-boot1.sock /tmp/guest-diag-provision-boot1.txt
  exit 1
fi
capture_guest_diagnostics /tmp/guest-diag-provision-boot1.txt

ssh_guest '
  set -euo pipefail
  ! test -e /etc/ssh/authorized_keys
  test -s /var/roothome/.ssh/authorized_keys
  test -s /var/lib/ssh/ssh_host_ed25519_key.pub
'
verify_mounts_and_selinux
wait_boot_health
test "$(ssh_guest 'cat /etc/machine-id')" = "$EXPECTED_MACHINE_ID"
test "$(ssh_guest 'hostname')" = "$EXPECTED_HOSTNAME"
wait_wg0 "$EXPECTED_WG_ADDR"
test "$(ssh_guest 'wg show wg0 public-key')" = "$EXPECTED_WG_PUBKEY"
ssh_guest '/usr/libexec/iota-site status | grep -qx "current: gens/1"'
ssh_guest 'cat /var/lib/ssh/ssh_host_ed25519_key.pub' | tee /tmp/provision-ssh-hostkey.txt

# Apply v1.0.1 update via systemd-sysupdate
ssh_guest "/usr/lib/systemd/systemd-sysupdate list --verify=no"
ssh_guest "/usr/lib/systemd/systemd-sysupdate update --verify=no"
ssh_guest "blkid -t PARTLABEL=iota_1.0.1"
ssh_guest 'ESP_DIR=$(bootctl --print-esp-path) && test -f "$ESP_DIR/EFI/Linux/iota_1.0.1_x86-64+3-0.efi"'

poweroff_guest "$QEMU_PID"
QEMU_PID=""

# Boot 2: v1.0.1 on Slot B; verify blessing and preserved provisioned state
start_qemu /tmp/serial-provision-boot2.log /tmp/serial-provision-boot2.sock

if [ "$SSH_VAR_LUKS" = "true" ]; then
  echo "Unlocking /var via initrd SSH for updated v1.0.1..."
  unlock_var_ssh 45 /tmp/serial-provision-boot2.log "$LUKS_PASSPHRASE"
fi

echo "Waiting for updated v1.0.1 guest SSH..."
if ! wait_ssh 60 /tmp/serial-provision-boot2.log; then
  capture_serial_diagnostics /tmp/serial-provision-boot2.sock /tmp/guest-diag-provision-boot2.txt
  exit 1
fi
capture_guest_diagnostics /tmp/guest-diag-provision-boot2.txt

BOOTED_VER=$(ssh_guest ". /usr/lib/os-release && echo \$IMAGE_VERSION")
echo "Provisioned Booted Version: $BOOTED_VER"
test "$BOOTED_VER" = "1.0.1"
ssh_guest "lsblk -s -no PARTLABEL /dev/mapper/root | grep -qx iota_1.0.1"

if ! verify_bless_boot; then
  echo "::group::bless-boot diagnostics"
  ssh_guest '
    systemctl status systemd-bless-boot.service --no-pager -l
    journalctl -b -u systemd-bless-boot.service --no-pager
    bootctl status --no-pager
    ausearch -m avc -ts boot 2>/dev/null || journalctl -b | grep -i avc || true
  ' || true
  echo "::endgroup::"
  exit 1
fi

verify_mounts_and_selinux
wait_boot_health
test "$(ssh_guest 'cat /etc/machine-id')" = "$EXPECTED_MACHINE_ID"
test "$(ssh_guest 'hostname')" = "$EXPECTED_HOSTNAME"
wait_wg0 "$EXPECTED_WG_ADDR"
test "$(ssh_guest 'wg show wg0 public-key')" = "$EXPECTED_WG_PUBKEY"
test "$(ssh_guest 'cat /var/lib/ssh/ssh_host_ed25519_key.pub')" = "$(cat /tmp/provision-ssh-hostkey.txt)"
ssh_guest '/usr/libexec/iota-site status | grep -qx "current: gens/1"'

poweroff_guest "$QEMU_PID"
QEMU_PID=""
