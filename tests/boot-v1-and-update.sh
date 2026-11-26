#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tests/lib.sh
source "$SCRIPT_DIR/lib.sh"

# Copy writable OVMF_VARS so UEFI boot entries persist across reboots
cp /usr/share/OVMF/OVMF_VARS_4M.fd ./ovmf_vars.fd

QEMU_PID=""
trap 'if [ -n "$QEMU_PID" ]; then kill "$QEMU_PID" 2>/dev/null || true; fi' EXIT

start_qemu /tmp/serial-boot1.log /tmp/serial-boot1.sock -no-reboot

if [ "${SSH_VAR_LUKS:-false}" = "true" ]; then
  echo "Unlocking /var via initrd SSH for v1.0.0..."
  unlock_var_ssh 45 /tmp/serial-boot1.log "$LUKS_PASSPHRASE"
fi

echo "Waiting for v1.0.0 guest SSH..."
if ! wait_ssh 60 /tmp/serial-boot1.log; then
  capture_serial_diagnostics /tmp/serial-boot1.sock /tmp/guest-diag-boot1.txt
  exit 1
fi
capture_guest_diagnostics /tmp/guest-diag-boot1.txt

# 1. Verify systemd-repart.service succeeded on boot (runs in initrd; skipped in real root when repart.d is initrd-only)
ssh_guest '
  set -euo pipefail
  systemctl status systemd-repart.service --no-pager || true
  test "$(systemctl show -p Result --value systemd-repart.service)" = "success"
  journalctl -b -u systemd-repart.service --no-pager | grep -q "Finished systemd-repart.service"
'

# 2. Verify the partition table actually has the '_empty' B slots and /var
ssh_guest "lsblk -o NAME,PARTLABEL,PARTTYPE,PARTUUID,SIZE"
ssh_guest "blkid -t PARTLABEL=_empty"

# 3. Verify persistent /var and /etc overlay are mounted and write test markers
verify_mounts_and_selinux
ssh_guest 'mkdir -p /var/lib && echo "var-persisted-from-1.0.0" > /var/lib/persist-test'
ssh_guest 'echo "etc-persisted-from-1.0.0" > /etc/persist-etc-test'

# Record how the ESP is mounted on the running v1.0.0 system (useful for debugging UUID mismatches)
ssh_guest 'findmnt /efi; blkid | grep -i vfat; cat /etc/fstab 2>/dev/null || true' | tee /tmp/guest-esp-v100.txt

# 4. Verify systemd-sysupdate sees the empty slot and the remote v1.0.1 update
ssh_guest "/usr/lib/systemd/systemd-sysupdate components"
ssh_guest "/usr/lib/systemd/systemd-sysupdate list --verify=no"

# 5. Run the update and verify '_empty' became 'iota_1.0.1' and boot-counted UKI exists
ssh_guest "/usr/lib/systemd/systemd-sysupdate update --verify=no"
ssh_guest "lsblk -o NAME,PARTLABEL,PARTUUID,SIZE"
ssh_guest 'ESP_DIR=$(bootctl --print-esp-path) && ls -la "$ESP_DIR/EFI/Linux/" && test -f "$ESP_DIR/EFI/Linux/iota_1.0.1_x86-64+3-0.efi"'

# Power off cleanly so filesystems and EFI vars flush
poweroff_guest "$QEMU_PID"
