#!/usr/bin/env bash
# Shared CI/test helper functions for building, inspecting, booting, and verifying images.

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SSH_OPTS=(
  -i /tmp/ci_id_ed25519
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o ConnectTimeout=3
  -p 2222
)

FATAL_RE='You are in emergency mode|Give root password for maintenance|Please configure the system|Kernel panic|Timed out waiting for device|Failed to start .*systemd-repart\.service'

ssh_guest() {
  ssh "${SSH_OPTS[@]}" root@127.0.0.1 "$@"
}

# wait_ssh <tries> <serial-log>
# Polls SSH, but bails out immediately if the serial log shows a fatal boot state.
wait_ssh() {
  local tries=$1 log=$2 i
  for ((i = 0; i < tries; i++)); do
    if ssh_guest true 2>/dev/null; then
      return 0
    fi
    if grep -qaE "$FATAL_RE" "$log" 2>/dev/null; then
      echo "::error::Fatal boot state detected in $log"
      grep -aE "$FATAL_RE" "$log" | head -n 10 || true
      return 2
    fi
    sleep 2
  done
  echo "::error::SSH never came up (see $log)"
  echo "---- last ssh -v attempt ----"
  ssh -v "${SSH_OPTS[@]}" root@127.0.0.1 true 2>&1 || true
  return 1
}

# unlock_var_ssh <tries> <serial-log> <passphrase>
# Connects to initrd sshd-unlock, answers passphrase prompts via PTY, and waits for switch-root EOF.
unlock_var_ssh() {
  local tries=$1 log=$2 passphrase=$3 i rc
  for ((i = 0; i < tries; i++)); do
    rc=0
    "$LIB_DIR/unlock-var-ssh.py" "$passphrase" || rc=$?
    if [ "$rc" -eq 0 ]; then
      return 0
    fi
    if grep -qaE "$FATAL_RE" "$log" 2>/dev/null; then
      echo "::error::Fatal boot state detected in $log while unlocking /var"
      grep -aE "$FATAL_RE" "$log" | head -n 10 || true
      return 2
    fi
    if [ "$rc" -eq 2 ]; then
      echo "::error::Timed out waiting for switch-root after answering passphrase prompts (see $log)"
      return 1
    fi
    sleep 2
  done
  echo "::error::Timed out unlocking /var via SSH (see $log)"
  return 1
}

# inspect_root <erofs-image-or-device> <label>
# Mounts a root image read-only and reports fstab, by-uuid references, and systemd units.
inspect_root() {
  local src=$1 label=$2
  echo "---- [$label] root contents ----"
  sudo mkdir -p /mnt/inspect
  if sudo mount -t erofs -o loop,ro "$src" /mnt/inspect 2>/dev/null; then
    echo "-- /etc/fstab --"
    cat /mnt/inspect/etc/fstab 2>/dev/null || echo "(no fstab)"
    echo "-- by-uuid references --"
    sudo grep -rnE "by-uuid" \
      /mnt/inspect/etc /mnt/inspect/usr/lib/systemd/system \
      /mnt/inspect/usr/lib/systemd/system-generators 2>/dev/null | head -n 50 || true
    echo "-- /etc/systemd/system --"
    ls -la /mnt/inspect/etc/systemd/system 2>/dev/null | head -n 40 || true
    sudo umount /mnt/inspect
  else
    echo "(could not mount $src as erofs)"
    file "$src" || true
  fi
}

# inspect_disk <raw-disk> <label>
inspect_disk() {
  local disk=$1 label=$2 loop p
  echo "==== [$label] disk layout ===="
  loop=$(sudo losetup -Pf --show "$disk")
  sudo sfdisk -d "$loop" || true
  for p in "${loop}"p*; do
    echo "-- $p --"
    sudo blkid "$p" || true
  done
  for p in "${loop}"p*; do
    if [ "$(sudo blkid -s TYPE -o value "$p" 2>/dev/null)" = "erofs" ]; then
      inspect_root "$p" "$label"
      break
    fi
  done
  sudo losetup -d "$loop"
}

# capture_guest_diagnostics <out-file>
# Captures failed systemd units, dbus-broker/homed journals, SELinux AVCs, and boot errors.
capture_guest_diagnostics() {
  local out=$1
  ssh_guest '
    echo "==== systemctl --failed ===="
    systemctl --failed --no-pager --no-legend || true
    echo "==== dbus-broker & systemd-homed status ===="
    systemctl status dbus-broker.service systemd-homed.service --no-pager -l || true
    echo "==== dbus-broker & systemd-homed journal ===="
    journalctl -b -u dbus-broker.service -u systemd-homed.service --no-pager || true
    echo "==== failed unit journals ===="
    systemctl --failed --no-pager --no-legend | while read -r u _; do
      [ -n "$u" ] || continue
      echo "--- $u ---"
      journalctl -b -u "$u" -n 50 --no-pager || true
    done
    echo "==== SELinux status & AVCs ===="
    getenforce || true
    ausearch -m avc -ts boot 2>/dev/null || grep -i avc /var/log/audit/audit.log 2>/dev/null || journalctl -b | grep -i avc || true
    echo "==== SELinux labels (/var, /etc, /run) ===="
    ls -ldZ /var /var/lib/iota /var/lib/ssh /var/lib/credstore.encrypted /etc /run /run/.etc-overlay /run/dbus 2>/dev/null || true
    echo "==== volatile /etc upperdir diff ===="
    find /run/.etc-overlay/upper -maxdepth 4 -ls 2>/dev/null || true
    echo "==== iota site & health status ===="
    /usr/libexec/iota-site status 2>/dev/null || true
    systemctl status iota-boot-health.service iota-site-apply.service iota-ssh-hostkeys.service --no-pager -l || true
    echo "==== journalctl -b -p err ===="
    journalctl -b -p err -n 200 --no-pager || true
  ' > "$out" 2>&1 || true
}

# capture_serial_diagnostics <sock> <out-file>
capture_serial_diagnostics() {
  local sock=$1 out=$2
  "$LIB_DIR/capture-serial-diagnostics.py" "$sock" "$out" || true
}

# start_qemu <serial-log> <serial-sock> [extra-qemu-args...]
# Launches QEMU in the background and sets QEMU_PID.
start_qemu() {
  local serial_log=$1 serial_sock=$2
  shift 2
  : > "$serial_log"
  rm -f "$serial_sock"

  qemu-system-x86_64 \
    -enable-kvm -cpu host -m 2G -smp 2 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file=./ovmf_vars.fd \
    -drive file=./disk-test.raw,format=raw,if=virtio \
    -net nic,model=virtio \
    -device i6300esb -watchdog-action reset \
    -net user,hostfwd=tcp:127.0.0.1:2222-:22 \
    -display none -monitor none \
    -chardev "socket,id=ser0,path=${serial_sock},server=on,wait=off,logfile=${serial_log},logappend=on" \
    -serial chardev:ser0 \
    "$@" &
  QEMU_PID=$!
}

# poweroff_guest <qemu-pid>
poweroff_guest() {
  local pid=$1 i
  ssh_guest "systemctl poweroff" || true
  for i in {1..45}; do
    kill -0 "$pid" 2>/dev/null || break
    sleep 2
  done
  wait "$pid" || true
}

# wait_for_serial_pattern <log> <start-line> <regex> <timeout-sec>
# Polls lines appended to <log> after <start-line> until <regex> matches or FATAL_RE triggers.
wait_for_serial_pattern() {
  local log=$1 start_line=$2 regex=$3 timeout=$4
  local deadline
  deadline=$(($(date +%s) + timeout))
  while true; do
    if tail -n "+$((start_line + 1))" "$log" 2>/dev/null | grep -qaE "$regex"; then
      return 0
    fi
    if tail -n "+$((start_line + 1))" "$log" 2>/dev/null | grep -qaE "$FATAL_RE"; then
      echo "::error::Fatal boot state detected in $log while waiting for '$regex'"
      tail -n "+$((start_line + 1))" "$log" | grep -aE "$FATAL_RE" | head -n 10 || true
      return 2
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
      echo "::error::Timed out waiting for '$regex' in $log (after line $start_line)"
      tail -n "+$((start_line + 1))" "$log" | tail -n 50 || true
      return 1
    fi
    sleep 1
  done
}

# wait_wg0 [expected-addr]
# Waits for the image-baked wg0 to exist with the provisioned address and port.
wait_wg0() {
  local expected_addr=${1:-10.100.0.10/24}
  if ! ssh_guest "EXPECTED_ADDR='$expected_addr' bash -s" <<'EOF'
    set -euo pipefail
    for _ in $(seq 1 20); do
      ip -br addr show wg0 2>/dev/null | grep -qF "$EXPECTED_ADDR" && break
      sleep 1
    done
    ip -br addr show wg0 | grep -qF "$EXPECTED_ADDR"
    test "$(wg show wg0 listen-port)" = 51820
    test -f /etc/credstore.encrypted/network.wireguard.private.wg0
EOF
  then
    ssh_guest 'networkctl status wg0 --no-pager || true; journalctl -b -u systemd-networkd --no-pager | tail -n 50' || true
    return 1
  fi
}

# wait_boot_health
# Waits for iota-boot-health.service to finish activating and succeed.
wait_boot_health() {
  ssh_guest '
    set -euo pipefail
    for _ in $(seq 1 45); do
      state=$(systemctl is-active iota-boot-health.service || true)
      case "$state" in
        active) exit 0 ;;
        failed)
          systemctl status iota-boot-health.service --no-pager -l || true
          journalctl -b -u iota-boot-health.service --no-pager || true
          exit 1
          ;;
      esac
      sleep 1
    done
    systemctl status iota-boot-health.service --no-pager -l || true
    exit 1
  '
}

verify_mounts_and_selinux() {
  if [ "${SSH_VAR_LUKS:-false}" = "true" ]; then
    ssh_guest 'test "$(findmnt -n -o SOURCE /var)" = "/dev/mapper/var"'
  else
    ssh_guest 'test "$(findmnt -n -o SOURCE /var)" = "$(blkid -t PARTLABEL=var -o device)"'
  fi
  ssh_guest 'test "$(findmnt -n -o FSTYPE /etc)" = "overlay"'
  ssh_guest 'findmnt -n -o OPTIONS /etc | grep -qF "upperdir=/run/.etc-overlay/upper"'
  ssh_guest 'getenforce | grep -qx Enforcing'
}
