# ι Iota

An immutable, A/B-updatable Linux operating system image based on **AlmaLinux 10**, built with [`mkosi`](https://github.com/systemd/mkosi), `systemd-repart`, `systemd-sysupdate`, `systemd-boot` Unified Kernel Images (UKIs), `erofs` + `dm-verity`, and **SELinux in Enforcing mode**.

> Originally forked from `castrojo/ubuntu-mkosi` as a sandbox for testing `mkosi`, A/B image layouts, and update hosting via Github releases; since redesigned around AlmaLinux 10, with an updated systemd from CentOS Hyperscale SIG, and patterns from `systemd/particleos`.

---

## Architecture & Key Features

### 1. Partition Layout & First-Boot Repartitioning

Iota builds a minimal, read-only Slot A image at build time and delegates Slot B and persistent storage creation to `systemd-repart` on first boot in the initrd.

> The initrd can be further customized by editing the files within mkosi.images/initrd

**Build-time partitions (`mkosi.repart/`):**
- **ESP (`00-esp.conf`)** — `vfat`, `512M`–`1G`, populated with `systemd-boot` and the UKI.
- **Root Verity (`11-root-verity.conf`)** — `root-verity` (`%M_%A_verity`), minimized hash tree for Slot A.
- **Root (`12-root.conf`)** — `erofs` (LZ4HC compressed, read-only, `%M_%A`), minimized and bound to `root-verity` via `VerityMatchKey=root`.

> In my testing Fedora and Alma's kernels did not have zstd compression for erofs enabled.

**First-boot initrd expansion (`mkosi.images/initrd/mkosi.extra/usr/lib/repart.d/` & `mkosi.extra/usr/lib/repart.d/`):**
On first boot, `systemd-repart.service` (ordered after `sysroot.mount` with `TMPDIR=/tmp`) expands the partition table into available disk space:
- Keeps Slot A (`00-esp.conf`, `11-root-verity.conf`, `12-root.conf` at `2G`).
- Creates empty Slot B partitions labeled `_empty`:
  - `21-root-verity.conf` (`64M`, `root-verity`)
  - `22-root.conf` (`2G`, `root`)
- Creates and formats persistent `/var` (`50-var.conf`, XFS, `Label=var`, `SizeMinBytes=300M`, `GrowFileSystem=yes`) with `/home`, `/etc`, `/.etc-work`, `/opt`, `/roothome`, `/mnt`, and `/srv`.

### 2. Immutable Root, Stateless `/etc` & Three-Tier State

- **Read-only `/`:** `/` is mounted from the `dm-verity` protected EROFS image (`/dev/mapper/root`).
- **Persistent `/var`:** Mounted in the initrd (`sysroot-var.mount`) and post-switch-root (`var.mount`) from `/dev/disk/by-partlabel/var`.
- **Stateless volatile `/etc`:** In the initrd, `/usr/libexec/mount-etc-overlay` (`mkosi.images/initrd/mkosi.extra/usr/libexec/mount-etc-overlay`) mounts an OverlayFS over `/sysroot/etc` whose `upperdir` and `workdir` live on a dedicated `tmpfs` (`/run/.etc-overlay`). Arbitrary writes to `/etc` never persist across reboots or leak across A/B rollbacks.
- **Explicit three-tier configuration split:**

| Tier | Examples | Storage | Rollback behaviour |
|------|----------|---------|--------------------|
| **1. Identity** (host-generated, never shipped) | WireGuard private key (`/etc/credstore.encrypted` → `/var/lib/credstore.encrypted`), SSH host key (`/var/lib/ssh/ssh_host_ed25519_key`), `/etc/machine-id` (`/var/lib/iota/machine-id`) | Persistent `/var` (bind-mounted/copied before `switch-root` or read directly via config) | Never rolled back |
| **2. Fleet policy** (same on every host, public-safe) | WireGuard topology & peer pubkeys (`50-wg0.{netdev,network}`), firewalld policies, fallback DHCP (`95-fallback-dhcp.network`), health gate (`health.conf`), systemd units | dm-verity `/usr` in each A/B slot | Rolls back atomically with the slot via `systemd-bless-boot` |
| **3. Site data** (per-host, can break networking) | Per-host WireGuard tunnel address, optional hostname | `/var/lib/iota/site/{gens,current,pending,pending.tries}` | Gated by 3-try trial/commit; falls back to `current` when tries hit 0 |

- **Persistent top-level directories:** `/home`, `/root`, `/mnt`, `/opt`, and `/srv` are symlinked into `var/*` (`var/home`, `var/roothome`, `var/mnt`, `var/opt`, `var/srv`) and initialized via `usr/lib/tmpfiles.d/50-var-dirs.conf`.
- **Stateless PAM stack:** `/etc/pam.d` is moved to `/usr/lib/pam.d` during image build, with obsolete `pam_shells.so` and `pam_securetty.so` lines removed.
- **Fallback control path:** `95-fallback-dhcp.network` configures DHCP + IPv6 RA on physical ethernet interfaces independently of WireGuard or per-host site data.

### 3. Atomic A/B Updates & Automatic Rollback

`mkosi.conf` produces split artifacts (`SplitArtifacts=uki,partitions`):
- Full compressed disk image: `iota_<version>_<arch>.raw.zst`
- Unified Kernel Image (with embedded initrd): `iota_<version>_<arch>.efi`
- Split verity-linked root & hash partitions: `iota_<version>_<arch>.root-<arch>.<uuid>.raw.zst` and `iota_<version>_<arch>.root-<arch>-verity.<uuid>.raw.zst`

Updates are managed by `systemd-sysupdate` using three transfer definitions in `mkosi.extra/usr/lib/sysupdate.d/`:
- `00-uki.transfer` — installs the UKI to `/EFI/Linux` on the ESP with `TriesLeft=3`, `TriesDone=0`, and `InstancesMax=2`.
- `10-root-verity.transfer` — writes the verity hash image to the spare `root-verity` slot (`%M_@v_verity`).
- `11-root.transfer` — writes the root EROFS image to the spare `root` slot (`%M_@v`).

All transfers set `ProtectVersion=%A` to prevent overwriting the currently booted version. On reboot:
1. `systemd-boot` selects the new `+3-0` UKI and decrements the try counter (`+2-1`).
2. Once the boot succeeds (`boot-complete.target`), `systemd-bless-boot.service` marks the entry good and strips the `+tries` suffix.
3. If the new version fails to complete boot and exhausts its tries (`+0-*`), `systemd-boot` automatically falls back to the previous working slot.

### 4. Security & Runtime Defaults

- **SELinux Enforcing:** `lsm=selinux` on the kernel command line, `SELINUX=enforcing` (`targeted` policy) in `/etc/selinux/config`, build-time labeling via `SELinuxRelabel=yes`, and first-boot `/var` labeling in `/usr/libexec/mount-etc-overlay` via `setfiles`.
- **Custom `iota_bless` SELinux module (`mkosi.extra/usr/share/selinux/iota/iota_bless.te`):** Compiled and installed at image build time (`checkmodule`, `semodule_package`, `semodule -n -i`) so `systemd-bless-boot` (`init_t`) can read, write, rename, and unlink UKI files on the `dosfs_t` ESP under Enforcing mode.
- **Minimal attack surface:** Package managers (`dnf`, `dnf-data`, `dnf-plugins-core`, `yum`, `dracut`) are removed from the runtime image (`RemovePackages=`).
- **Base services & tools:** `systemd-networkd`, `systemd-resolved`, `systemd-timesyncd`, socket-activated `sshd.socket`, `firewalld`, `podman`, `wireguard-tools`, `fish`, `vim` (`EDITOR`/`VISUAL`), and `bat` (`PAGER`).
- **Repositories:** AlmaLinux 10 + EPEL + CentOS Stream Hyperscale SIG `systemd` repo (`mkosi.sandbox/etc/yum.repos.d/centos-hyperscale-systemd.repo`).

### 5. WireGuard Mesh, Site Data Trial/Commit & Health Gate

Release artifacts are public, so WireGuard and per-host site settings are split across the three tiers:

| What | Where | Versioned by |
|------|-------|--------------|
| Interface, listen port, full peer list (public keys, endpoints, `AllowedIPs`) | `mkosi.extra/usr/lib/systemd/network/50-wg0.{netdev,network}` (dm-verity `/usr`) | A/B slots, rolls back with `systemd-bless-boot` |
| Inbound `51820/udp` | `mkosi.extra/usr/lib/firewalld/policies/iota-wireguard-ingress.xml` | A/B slots |
| Per-host private key | `/etc/credstore.encrypted/network.wireguard.private.wg0` (bind-mounted from `/var/lib/credstore.encrypted`) | Tier 1 `/var` (never rolled back) |
| Per-host tunnel address & optional hostname | `/var/lib/iota/site/gens/<N>/` staged to `/etc/systemd/network/50-wg0.network.d/10-address.conf` (and `/etc/hostname`) on boot | Tier 3 trial/commit (`iota-site`) |

- `50-wg0.netdev` uses `PrivateKey=@network.wireguard.private.wg0`. `systemd-networkd.service` already has `ImportCredential=network.wireguard.*`, which reads from the bind-mounted `/etc/credstore.encrypted`, so no unit drop-in is needed.
- `[Match] Credential=network.wireguard.private.wg0` makes `wg0` inert on hosts that haven't been provisioned.
- You can list every node, including the local one, as a `[WireGuardPeer]`. The kernel silently ignores a peer whose public key matches its own, so one peer list works on every host.

**Initial provisioning (once per host):**

```bash
/usr/libexec/iota-wg-provision 10.100.0.3/24
```

This generates the encrypted key (no PCR binding, so UKI and firmware updates don't lock it out), commits generation `gens/1` as `current`, restarts `systemd-networkd`, and prints the `[WireGuardPeer]` block to commit into `50-wg0.netdev`. Peers learn about the new node from the next release.

**Updating per-host site data with trial/commit (`iota-site`):**

Once `current` exists, subsequent `iota-wg-provision <new-addr/prefix>` calls (or `/usr/libexec/iota-site stage --wg-address ... --hostname ... --tries 3`) stage a `pending` generation under `/var/lib/iota/site/` without overwriting `current`. On reboot:
1. `iota-site-apply.service` decrements `pending.tries` and stages `pending` into volatile `/etc` (or falls back to `current` and records `last-failed` once `pending.tries` hits `0`).
2. `iota-boot-health.service` runs before `boot-complete.target` (and at `multi-user.target`) and verifies:
   - `sshd.socket` (or `sshd.service`) and `firewalld.service` are active,
   - a default route exists,
   - at least one HTTP/TLS probe succeeds across the configured `sysupdate` URL or fallback probes (`https://1.1.1.1/`, `https://8.8.8.8/`) — preventing a GitHub release outage from causing false rollbacks while catching broken routing or egress firewall rules, and
   - if WireGuard is provisioned (`network.wireguard.private.wg0` exists), `wg0` is present with a positive listen port, public key, and all configured drop-in addresses.
3. On pass, `iota-boot-health` commits `pending` → `current`, allowing `systemd-bless-boot.service` to bless the OS slot.
4. On fail, `iota-boot-health` reboots **only** if `pending` site config (`site_mode=pending`) or an unblessed OS slot (`indeterminate`) is being trialed; on an already-blessed slot with no pending site config, it logs the error without rebooting so an isolated host does not reboot-loop.

Available subcommands: `/usr/libexec/iota-site {stage|commit|rollback|status}`.

> [!NOTE]
> Rollbacks revert topology (Tier 2) and pending site data (Tier 3), not identity keys (Tier 1). The WireGuard private key stays in `/var/lib/credstore.encrypted`. If a node rotates its key (`--rotate`), its peers need a release that contains the new public key, and rolling a peer back to an older release resurrects the old public key for that node from the older image.

### 6. Provisioning Without a Private Release Channel (Hetzner & QEMU)

Per-host SSH authorization and secrets stay out of the public image and release artifacts:

1. **Hetzner Rescue System + ESP credentials (`/loader/credentials/`):**
   Boot the Hetzner server into the Rescue System, write the decompressed raw disk image to the target block device, mount the ESP (partition 1), and drop `.cred` files into `<ESP>/loader/credentials/`:
   - `ssh.authorized_keys.root.cred` (imported automatically by `systemd-stub` and written by `/usr/lib/tmpfiles.d/provision.conf` to `/root/.ssh/authorized_keys`, which resolves to persistent `/var/roothome/.ssh/authorized_keys` labeled `ssh_home_t`).
   - Optionally `passwd.hashed-password.root.cred` for root console login.
   Unmount, reboot out of Rescue Mode, then run `/usr/libexec/iota-wg-provision` via SSH or the Hetzner web console.
2. **Hetzner Cloud `cloud-init` user-data:**
   Hetzner's cloud metadata service (`169.254.169.254` / `cidata`) targets images shipping Python `cloud-init`, which this minimal image intentionally omits. Using Rescue Mode + ESP `/loader/credentials/*.cred` (or the Hetzner web console to stage credentials once onto `/root/.ssh/authorized_keys` or `/var/lib/credstore.encrypted`) keeps provisioning simple without pulling `cloud-init` and Python dependencies into the image.
3. **QEMU / CI (`SMBIOS type=11`):**
   Pass credentials directly via QEMU SMBIOS OEM strings without modifying the disk image:
   ```bash
   KEY_B64=$(base64 -w0 ~/.ssh/id_ed25519.pub)
   -smbios "type=11,value=io.systemd.credential.binary:ssh.authorized_keys.root=${KEY_B64}"
   ```
4. **Offline Installer CLI from Arch Linux / Rescue Mode (`tools/iota-provision`):**
   Build and run the `iota-provision` Rust CLI as root from an Arch Linux live ISO, workstation, or Rescue environment to flash a public `.raw.zst`/`.raw` image (optional via `--image`), create the Slot B + `/var` partitions with `systemd-repart`, and pre-provision LUKS, SSH keys (`<ESP>/unlock/authorized_keys` and `/var/roothome/.ssh/authorized_keys`), target `machine-id`, encrypted WireGuard credential, and Tier 3 site config (`wg0` address and hostname) before first boot:
   ```bash
   just provision \
     --image iota_20260101_x86-64.raw.zst \
     --target /dev/nvme0n1 \
     --ssh-key ~/.ssh/id_ed25519.pub \
     --wg-address 10.100.0.42/24 \
     --hostname node-42
   ```
   (Or build and run directly with `cargo build --release -p iota-provision && sudo ./target/release/iota-provision ...`.)

### 7. Why In-Image Config Over `confext` (`systemd 262.2` Evaluation)

Shipping fleet policy directly in dm-verity `/usr` was chosen over `systemd-confext` after inspecting `systemd 262.2` and `systemd-stub` in the built image:

1. **Initrd-only `.extra.d` scope:** `systemd-stub` (262) strips `+tries-done` suffixes (`foo+3-0.efi` → `foo.efi.extra.d/`) and unpacks `*.confext.raw` into `/.extra/confext/`, which is consumed by `systemd-confext-initrd.service`. That unit has `Conflicts=initrd-switch-root.target` (unmerging at switch-root), whereas `systemd-confext-sysroot.service` and real-root `systemd-confext.service` only read `/var/lib/confexts`, `/usr/local/lib/confexts`, `/usr/lib/confexts`, and `/run/confexts` — not `/.extra/confext/`.
2. **No automatic rollback for decoupled `confext` updates:** Once a UKI is blessed (`foo.efi`), updating a `.confext.raw` independently does not trigger `systemd-boot` boot counting or fallback unless you also re-arm UKI boot counting or build a parallel confext boot counter and OS-version compatibility validation.
3. **`systemd-sysupdate` path constraints:** `[Target] Path=` in `sysupdate.d(5)` is static and cannot interpolate `@v` into a parent directory such as `/EFI/Linux/iota_@v_x86-64.efi.extra.d/`.
4. **Unnecessary layer complexity:** In-image `/usr/lib/` already gets dm-verity + A/B UKI rollback with zero extra overlay layers or SELinux/signing pipelines.


---

## Repository Structure

```text
.
├── mkosi.conf                          # Main AlmaLinux 10 + Iota image definition
├── mkosi.postinst.chroot               # PAM cleanup, GPG trust, symlinks, iota_bless SELinux module, os-release/issue
├── mkosi.repart/                       # Build-time disk image partition definitions (ESP, root-verity, root)
├── mkosi.sandbox/                      # Sandbox repo & GPG key for CentOS Hyperscale SIG systemd
├── mkosi.extra/                        # Files injected into the root image
│   ├── efi/loader/loader.conf          # systemd-boot loader config
│   ├── etc/
│   │   ├── kernel/tries                # Initial boot-count tries (3)
│   │   ├── profile.d/                  # Default EDITOR=vim, PAGER=bat
│   │   ├── selinux/config              # SELINUX=enforcing, SELINUXTYPE=targeted
│   │   └── systemd/import-pubring.gpg  # sysupdate verification keyring
│   └── usr/
│       ├── lib/
│       │   ├── repart.d/               # Runtime partition definitions (Slot A, _empty Slot B, /var)
│       │   ├── systemd/                # var.mount & service presets (networkd, sshd.socket, bless-boot)
│       │   ├── sysupdate.d/            # Transfer definitions for UKI, root-verity, and root
│       │   └── tmpfiles.d/             # Persistent /var directory layout
│       └── share/
│           ├── keyrings/               # sysupdate release GPG keyring
│           └── selinux/iota/           # iota_bless.te SELinux module source
├── mkosi.images/
│   └── initrd/                         # Dedicated CPIO initrd image definition
│       ├── mkosi.conf                  # Initrd packages (systemd, cryptsetup, veritysetup, xfsprogs, etc.)
│       ├── mkosi.postinst.chroot       # Enables sysroot-var.mount and sysroot-etc-overlay.service
│       ├── mkosi.finalize              # Strips caches/docs from initrd
│       └── mkosi.extra/                # Initrd repart.d/, mount-etc-overlay, systemd-repart override
├── mkosi.profiles/                     # Inherited upstream mkosi profile drop-ins (not enabled by default)
└── .github/workflows/
    ├── ci.yml                          # Full QEMU/OVMF E2E test (repart, sysupdate A->B, bless-boot, rollback)
    ├── build.yml                       # Runs tests, builds signed artifacts, publishes GitHub Release
    └── shell.yml                       # Checks latest release RPM DB for upstream updates & triggers build
```

---

## Building & Booting Locally

### Prerequisites

Install `mkosi` (CI uses `systemd/mkosi@v27`) along with virtualization, partitioning, filesystem, and SELinux tools on the build host:

- `mkosi` and `bubblewrap`
- `systemd-boot`, `systemd-repart`, `systemd-ukify`
- `erofs-utils`, `xfsprogs`, `zstd`, `mtools`, `binutils`
- `policycoreutils`, `python3-pefile`, `dnf`
- `qemu-system-x86`, `qemu-utils`, `ovmf` (for local VM testing)

### Build the Image

Inspect the resolved configuration:

```bash
mkosi summary
```

Build the initrd and main disk image (`RootPassword=root` is set in `mkosi.conf` for local testing):

```bash
sudo mkosi --image-version=1.0.0 --force build
```

Build artifacts are written to `mkosi.output/`, including:
- `iota_1.0.0_x86-64.raw.zst` (compressed GPT disk image)
- `iota_1.0.0_x86-64.efi` (Unified Kernel Image)
- `iota_1.0.0_x86-64.root-x86-64.<uuid>.raw.zst` and `iota_1.0.0_x86-64.root-x86-64-verity.<uuid>.raw.zst`

### Boot in QEMU (UEFI)

Decompress the disk image and add free space (`+6G` or more) so `systemd-repart` can create the `_empty` Slot B partitions and the XFS `/var` partition on first boot:

```bash
zstd -d mkosi.output/iota_1.0.0_x86-64.raw.zst -o disk-test.raw
qemu-img resize -f raw disk-test.raw +6G
```

Boot with OVMF (copying `OVMF_VARS_4M.fd` so EFI variables persist across reboots):

```bash
cp /usr/share/OVMF/OVMF_VARS_4M.fd ./ovmf_vars.fd
qemu-system-x86_64 \
  -enable-kvm -cpu host -m 2G -smp 2 \
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,file=./ovmf_vars.fd \
  -drive file=./disk-test.raw,format=raw,if=virtio \
  -net nic,model=virtio \
  -net user,hostfwd=tcp:127.0.0.1:2222-:22 \
  -nographic
```

> Note: `mkosi.conf` does not enable serial console output by default (`logo.nologo lsm=selinux`); pass `console=ttyS0,115200n8` on the kernel command line if you want serial console output in `-nographic` mode.

---

## Verifying State, Updating & Rollback

Once booted into the VM:

```bash
# Verify SELinux, OS identity, partition layout, and mounts
getenforce
cat /usr/lib/os-release
lsblk -o NAME,PARTLABEL,PARTUUID,MOUNTPOINTS,SIZE
findmnt / /var /etc

# Inspect systemd-sysupdate components and available releases
/usr/lib/systemd/systemd-sysupdate components
/usr/lib/systemd/systemd-sysupdate list

# Stage an update into the _empty Slot B and install the boot-counted UKI
/usr/lib/systemd/systemd-sysupdate update
systemctl reboot
```

After rebooting into the updated slot:

```bash
# Verify boot-blessing status and active slot
/usr/lib/systemd/systemd-bless-boot status
bootctl status
```

If a new UKI fails to reach `boot-complete.target` before its 3 boot tries expire, `systemd-boot` automatically falls back to the previous slot while keeping Tier 1 identity on `/var` and resetting `/etc` cleanly from the image + active site generation.

---

## CI/CD Workflows

- **Verify (`ci.yml`):** Runs on pull requests and manual dispatch. Builds `1.0.0`, `1.0.1`, and bad-fleet-policy `1.0.2`; resizes the disk by `+6G`; boots `1.0.0` in QEMU/OVMF; validates `systemd-repart`, `_empty` Slot B, `/var`, volatile `/etc` overlay (`upperdir=/run/.etc-overlay/upper`), Tier 1 identity (`machine-id`, `/var/lib/ssh` host key, encrypted WireGuard key), and SELinux `Enforcing`; verifies `wg0` stays inert unprovisioned then comes up after `iota-wg-provision`; updates to `1.0.1`, verifies `systemd-bless-boot` and that non-allowlisted `/etc` writes do not persist across reboot; verifies bad pending site data rolls back to `current`; and updates to `1.0.2` (with an egress-blocking firewall policy) to verify `iota-boot-health` rejects blessing and `systemd-boot` rolls back to `iota_1.0.1`.
- **Build & Release (`build.yml`):** Runs on pushes to `main`, `workflow_dispatch`, or calls from `shell.yml`. Runs `ci.yml`, builds date-versioned artifacts (`YYYYMMDD` / `YYYYMMDD.N`), signs `SHA256SUMS` with GPG, and publishes a GitHub Release.
- **Update Check (`shell.yml`):** On `workflow_dispatch`, downloads the latest released root partition image, mounts it, and runs `dnf check-update` against `/usr/lib/sysimage/rpm/rpmdb.sqlite` inside an `almalinux` container. If package updates are available, it invokes `build.yml` with generated package diff release notes.
