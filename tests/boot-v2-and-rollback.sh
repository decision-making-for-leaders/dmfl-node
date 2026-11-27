#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tests/lib.sh
source "$SCRIPT_DIR/lib.sh"

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

QEMU_PID=""
trap 'if [ -n "$QEMU_PID" ]; then kill "$QEMU_PID" 2>/dev/null || true; fi' EXIT

# No -no-reboot here: the rollback test relies on the guest rebooting itself.
start_qemu /tmp/serial-boot2.log /tmp/serial-boot2.sock

if [ "${SSH_VAR_LUKS:-false}" = "true" ]; then
  echo "Unlocking /var via initrd SSH for v1.0.1..."
  unlock_var_ssh 45 /tmp/serial-boot2.log "$LUKS_PASSPHRASE"
fi

echo "Waiting for v1.0.1 guest SSH..."
if ! wait_ssh 60 /tmp/serial-boot2.log; then
  capture_serial_diagnostics /tmp/serial-boot2.sock /tmp/guest-diag-boot2.txt
  exit 1
fi
capture_guest_diagnostics /tmp/guest-diag-boot2.txt

ssh_guest "lsblk -o NAME,PARTLABEL,PARTUUID,MOUNTPOINTS,SIZE"
BOOTED_VER=$(ssh_guest ". /usr/lib/os-release && echo \$IMAGE_VERSION")
echo "Booted Version: $BOOTED_VER"
test "$BOOTED_VER" = "1.0.1"

# Verify /dev/mapper/root is backed by the iota_1.0.1 partition (Slot B)
ssh_guest "lsblk -s -no PARTLABEL /dev/mapper/root | grep -qx iota_1.0.1"

# Phase 1: Verify persistent /var, stateless volatile /etc, and Tier 1 identity across A -> B switch
verify_mounts_and_selinux
ssh_guest 'test "$(cat /var/lib/persist-test)" = "var-persisted-from-1.0.0"'
ssh_guest 'test ! -e /etc/ephemeral-etc-test'
test "$(ssh_guest 'cat /etc/machine-id')" = "$(cat /tmp/machine-id-v100.txt)"
test "$(ssh_guest 'cat /var/lib/ssh/ssh_host_ed25519_key.pub')" = "$(cat /tmp/ssh-hostkey-v100.txt)"
ssh_guest '! ls /etc/ssh/ssh_host_* >/dev/null 2>&1'

# Verify wg0 comes up on the new slot: topology from the 1.0.1 image, private
# key + address from persistent Tier 1 / Tier 3 state provisioned on 1.0.0.
wait_wg0 10.100.0.10/24
test "$(ssh_guest 'wg show wg0 public-key')" = "$(cat /tmp/wg0-pubkey-v100.txt)"
wait_boot_health

# Verify systemd-boot decremented +3-0 -> +2-1 and systemd-bless-boot marks it good
if ! verify_bless_boot; then
  echo "::group::bless-boot diagnostics"
  ssh_guest '
    systemctl status systemd-bless-boot.service --no-pager -l
    journalctl -b -u systemd-bless-boot.service --no-pager
    bootctl status --no-pager
    ls -la /sys/firmware/efi/efivars | grep -iE "loader|bootcount" || true
    cat /sys/firmware/efi/efivars/LoaderBootCountPath-* 2>/dev/null | tr -d "\0"; echo
    findmnt /boot /efi || true
    ls -ladZ "$(bootctl --print-esp-path)/EFI/Linux/" || true
    ausearch -m avc -ts boot 2>/dev/null || journalctl -b | grep -i avc || true
    /usr/lib/systemd/systemd-bless-boot status || true
  ' || true
  echo "::endgroup::"
  exit 1
fi

# Phase 2: Bad site data (invalid tunnel address) in a pending generation falls back to current
ssh_guest '/usr/libexec/iota-site stage --wg-address 10.100.0.999/24 --tries 1 --no-validate'
echo "Rebooting into bad pending site generation to verify automatic fallback to current..."
LINE=$(wc -l < /tmp/serial-boot2.log)
echo "=== CI: Phase 2 bad site data reboot ===" >> /tmp/serial-boot2.log
ssh_guest "touch /run/awaiting-rollback && systemctl reboot" || true
for _ in {1..30}; do
  ssh_guest "test -f /run/awaiting-rollback" 2>/dev/null || break
  sleep 1
done

if [ "${SSH_VAR_LUKS:-false}" = "true" ]; then
  echo "Unlocking /var via initrd SSH for bad site config trial..."
  unlock_var_ssh 45 /tmp/serial-boot2.log "$LUKS_PASSPHRASE"
fi
wait_for_serial_pattern /tmp/serial-boot2.log "$LINE" 'iota-boot-health: FAIL on pending site config' 120
LINE=$(wc -l < /tmp/serial-boot2.log)
wait_for_serial_pattern /tmp/serial-boot2.log "$LINE" 'Linux version' 45
LINE=$(wc -l < /tmp/serial-boot2.log)

if [ "${SSH_VAR_LUKS:-false}" = "true" ]; then
  echo "Unlocking /var via initrd SSH after site-data fallback..."
  unlock_var_ssh 45 /tmp/serial-boot2.log "$LUKS_PASSPHRASE"
fi
if ! wait_ssh 60 /tmp/serial-boot2.log; then
  capture_serial_diagnostics /tmp/serial-boot2.sock /tmp/guest-diag-boot2.txt
  exit 1
fi

test "$(ssh_guest '. /usr/lib/os-release && echo $IMAGE_VERSION')" = "1.0.1"
wait_boot_health
ssh_guest '
  set -euo pipefail
  status=$(/usr/libexec/iota-site status)
  echo "$status"
  echo "$status" | grep -qx "current: gens/1"
  echo "$status" | grep -qx "pending: (none)"
  echo "$status" | grep -qE "^last-failed: 2 exhausted"
'
wait_wg0 10.100.0.10/24
test "$(ssh_guest 'wg show wg0 public-key')" = "$(cat /tmp/wg0-pubkey-v100.txt)"

# Phase 3: Bad fleet policy in v1.0.2 fails health gate and rolls back to v1.0.1
rm -f /tmp/update-srv/*
cp /tmp/update-srv-v102/* /tmp/update-srv/
ssh_guest "/usr/lib/systemd/systemd-sysupdate update --verify=no"
ssh_guest "blkid -t PARTLABEL=iota_1.0.2"
ssh_guest "blkid -t PARTLABEL=iota_1.0.2_verity"
ssh_guest 'ESP_DIR=$(bootctl --print-esp-path) && test -f "$ESP_DIR/EFI/Linux/iota_1.0.2_x86-64+3-0.efi"'

echo "Rebooting into bad fleet policy v1.0.2 (+3-0) to verify health-gated slot rollback to v1.0.1..."
LINE=$(wc -l < /tmp/serial-boot2.log)
echo "=== CI: Phase 3 bad fleet policy v1.0.2 reboot ===" >> /tmp/serial-boot2.log
ssh_guest "touch /run/awaiting-rollback && systemctl reboot" || true
for _ in {1..30}; do
  ssh_guest "test -f /run/awaiting-rollback" 2>/dev/null || break
  sleep 1
done

for try in 1 2 3; do
  if [ "${SSH_VAR_LUKS:-false}" = "true" ]; then
    echo "Unlocking /var via initrd SSH for doomed v1.0.2 try $try..."
    unlock_var_ssh 45 /tmp/serial-boot2.log "$LUKS_PASSPHRASE"
  fi
  wait_for_serial_pattern /tmp/serial-boot2.log "$LINE" 'iota-boot-health: FAIL on unblessed OS slot' 120
  LINE=$(wc -l < /tmp/serial-boot2.log)
  wait_for_serial_pattern /tmp/serial-boot2.log "$LINE" 'Linux version' 45
  LINE=$(wc -l < /tmp/serial-boot2.log)
done

if [ "${SSH_VAR_LUKS:-false}" = "true" ]; then
  echo "Unlocking /var via initrd SSH for rolled-back v1.0.1..."
  unlock_var_ssh 45 /tmp/serial-boot2.log "$LUKS_PASSPHRASE"
fi
if ! wait_ssh 75 /tmp/serial-boot2.log; then
  capture_serial_diagnostics /tmp/serial-boot2.sock /tmp/guest-diag-boot2.txt
  exit 1
fi

ROLLBACK_VER=$(ssh_guest ". /usr/lib/os-release && echo \$IMAGE_VERSION")
echo "Rolled-back Booted Version: $ROLLBACK_VER"
test "$ROLLBACK_VER" = "1.0.1"

# Verify fallback booted Slot B (iota_1.0.1), v1.0.2 exhausted tries (+0-3), and state stayed intact
ssh_guest "lsblk -s -no PARTLABEL /dev/mapper/root | grep -qx iota_1.0.1"
ssh_guest 'ESP_DIR=$(bootctl --print-esp-path) && ls -la "$ESP_DIR/EFI/Linux/" && test -f "$ESP_DIR/EFI/Linux/iota_1.0.2_x86-64+0-3.efi"'
verify_mounts_and_selinux
ssh_guest 'test "$(cat /var/lib/persist-test)" = "var-persisted-from-1.0.0"'
test "$(ssh_guest 'cat /etc/machine-id')" = "$(cat /tmp/machine-id-v100.txt)"
test "$(ssh_guest 'cat /var/lib/ssh/ssh_host_ed25519_key.pub')" = "$(cat /tmp/ssh-hostkey-v100.txt)"
wait_wg0 10.100.0.10/24
test "$(ssh_guest 'wg show wg0 public-key')" = "$(cat /tmp/wg0-pubkey-v100.txt)"
wait_boot_health

poweroff_guest "$QEMU_PID"
