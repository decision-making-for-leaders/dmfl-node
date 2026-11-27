use std::env;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{symlink, FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::{Config, LuksMode};
use crate::guards::{LoopGuard, LuksGuard, MountGuard, TempDirGuard, TtyEchoGuard};
use crate::validate::{
    decode_base64, generate_repart_configs, parse_lsblk_partitions, strip_passphrase_endings,
    validate_machine_id,
};

const X25519_PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x6e, 0x04, 0x22, 0x04, 0x20,
];

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn effective_uid() -> Result<u32, String> {
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("failed to read /proc/self/status: {e}"))?;
    for line in status.lines() {
        if line.starts_with("Uid:") {
            let eff = line
                .split_whitespace()
                .nth(2)
                .ok_or_else(|| "malformed Uid line in /proc/self/status".to_string())?;
            return eff
                .parse::<u32>()
                .map_err(|e| format!("failed to parse effective UID '{eff}': {e}"));
        }
    }
    Err("Uid line not found in /proc/self/status".to_string())
}

fn command_exists(name: &str) -> bool {
    let Some(path_var) = env::var_os("PATH") else {
        return false;
    };
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if let Ok(meta) = fs::metadata(&candidate) {
            if meta.is_file() && (meta.permissions().mode() & 0o111 != 0) {
                return true;
            }
        }
    }
    false
}

fn require_command(name: &str) -> Result<(), String> {
    if command_exists(name) {
        Ok(())
    } else {
        Err(format!("required command '{name}' not found in PATH"))
    }
}

fn write_file_atomic(
    dest: &Path,
    data: &[u8],
    mode: u32,
    ignore_chmod_err: bool,
) -> Result<(), String> {
    let parent = dest
        .parent()
        .ok_or_else(|| format!("path '{}' has no parent directory", dest.display()))?;
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid file name '{}'", dest.display()))?;
    let tmp = parent.join(format!(".{name}.tmp.{}", std::process::id()));

    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .map_err(|e| format!("failed to create '{}': {e}", tmp.display()))?;
        f.write_all(data)
            .map_err(|e| format!("failed to write '{}': {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("failed to sync '{}': {e}", tmp.display()))?;
    }

    if let Err(e) = fs::set_permissions(&tmp, Permissions::from_mode(mode)) {
        if !ignore_chmod_err {
            let _ = fs::remove_file(&tmp);
            return Err(format!("failed to chmod '{}': {e}", tmp.display()));
        }
    }

    if let Err(e) = fs::rename(&tmp, dest) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("failed to install '{}': {e}", dest.display()));
    }
    Ok(())
}

fn create_dir_mode(path: &Path, mode: u32) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|e| format!("failed to create directory '{}': {e}", path.display()))?;
    fs::set_permissions(path, Permissions::from_mode(mode))
        .map_err(|e| format!("failed to chmod directory '{}': {e}", path.display()))?;
    Ok(())
}

fn install_authorized_keys(
    keys: &[PathBuf],
    dest: &Path,
    mode: u32,
    ignore_chmod_err: bool,
) -> Result<(), String> {
    let mut combined = String::new();
    for key_path in keys {
        let content = fs::read_to_string(key_path)
            .map_err(|e| format!("failed to read SSH key '{}': {e}", key_path.display()))?;
        for line in content.lines() {
            if !line.is_empty() {
                combined.push_str(line);
                combined.push('\n');
            }
        }
    }
    write_file_atomic(dest, combined.as_bytes(), mode, ignore_chmod_err)
}

fn prompt_yes_no(target: &Path, image: &Path) -> Result<(), String> {
    let mut tty_w = OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .map_err(|_| {
            format!(
                "refusing to overwrite block device '{}' without --yes or an interactive tty",
                target.display()
            )
        })?;
    let tty_r = File::open("/dev/tty").map_err(|_| {
        format!(
            "refusing to overwrite block device '{}' without --yes or an interactive tty",
            target.display()
        )
    })?;
    write!(
        tty_w,
        "About to overwrite block device '{}' with '{}'. Type 'yes' to continue: ",
        target.display(),
        image.display()
    )
    .map_err(|e| format!("failed to write prompt to /dev/tty: {e}"))?;
    tty_w
        .flush()
        .map_err(|e| format!("failed to flush /dev/tty: {e}"))?;

    let mut line = String::new();
    BufReader::new(tty_r)
        .read_line(&mut line)
        .map_err(|e| format!("failed to read response from /dev/tty: {e}"))?;
    if strip_passphrase_endings(&line) != "yes" {
        return Err("aborted by user".to_string());
    }
    Ok(())
}

fn flash_image(image: &Path, target: &Path, is_block: bool, assume_yes: bool) -> Result<(), String> {
    if is_block && !assume_yes {
        prompt_yes_no(target, image)?;
    }
    println!("Flashing '{}' to '{}'...", image.display(), target.display());

    let is_zst = image
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e == "zst");

    if is_zst {
        require_command("zstd")?;
        if is_block {
            let mut zstd_child = Command::new("zstd")
                .arg("-dc")
                .arg(image)
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("failed to spawn zstd: {e}"))?;
            let zstd_out = zstd_child
                .stdout
                .take()
                .ok_or_else(|| "failed to capture zstd stdout".to_string())?;
            let dd_status = Command::new("dd")
                .arg(format!("of={}", target.display()))
                .args(["bs=4M", "iflag=fullblock", "conv=fsync", "status=progress"])
                .stdin(Stdio::from(zstd_out))
                .status()
                .map_err(|e| format!("failed to run dd: {e}"))?;
            let zstd_status = zstd_child
                .wait()
                .map_err(|e| format!("failed waiting for zstd: {e}"))?;
            if !zstd_status.success() {
                return Err("zstd decompression failed".to_string());
            }
            if !dd_status.success() {
                return Err("dd write failed".to_string());
            }
        } else {
            let out_file = File::create(target)
                .map_err(|e| format!("failed to create '{}': {e}", target.display()))?;
            let status = Command::new("zstd")
                .arg("-dc")
                .arg(image)
                .stdout(Stdio::from(out_file))
                .status()
                .map_err(|e| format!("failed to run zstd: {e}"))?;
            if !status.success() {
                return Err("zstd decompression failed".to_string());
            }
        }
    } else {
        let status = Command::new("dd")
            .arg(format!("if={}", image.display()))
            .arg(format!("of={}", target.display()))
            .args(["bs=4M", "conv=fsync", "status=progress"])
            .status()
            .map_err(|e| format!("failed to run dd: {e}"))?;
        if !status.success() {
            return Err("dd image flash failed".to_string());
        }
    }

    let _ = Command::new("sync").status();
    let _ = Command::new("udevadm").arg("settle").status();
    Ok(())
}

fn read_tty_secret(prompt: &str) -> Result<String, String> {
    let mut tty_w = OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .map_err(|_| {
            "no /dev/tty available for LUKS passphrase prompt; pass --luks-passphrase-file, --luks-tpm2, or --no-luks"
                .to_string()
        })?;
    let tty_r = File::open("/dev/tty").map_err(|_| {
        "no /dev/tty available for LUKS passphrase prompt; pass --luks-passphrase-file, --luks-tpm2, or --no-luks"
            .to_string()
    })?;

    write!(tty_w, "{prompt}").map_err(|e| format!("failed to write to /dev/tty: {e}"))?;
    tty_w
        .flush()
        .map_err(|e| format!("failed to flush /dev/tty: {e}"))?;

    let _echo_guard = TtyEchoGuard::disable_echo()?;
    let mut line = String::new();
    let read_res = BufReader::new(tty_r).read_line(&mut line);
    drop(_echo_guard);
    let _ = writeln!(tty_w);

    read_res.map_err(|e| format!("failed to read passphrase from /dev/tty: {e}"))?;
    Ok(strip_passphrase_endings(&line).to_string())
}

fn prepare_luks_keyfile(mode: &LuksMode, keyfile_path: &Path) -> Result<bool, String> {
    match mode {
        LuksMode::PassphraseFile(path) => {
            let raw = fs::read_to_string(path).map_err(|e| {
                format!("failed to read LUKS passphrase file '{}': {e}", path.display())
            })?;
            let pass = strip_passphrase_endings(&raw);
            if pass.is_empty() {
                return Err(format!(
                    "LUKS passphrase file '{}' is empty",
                    path.display()
                ));
            }
            write_file_atomic(keyfile_path, pass.as_bytes(), 0o600, false)?;
            Ok(true)
        }
        LuksMode::Prompt => loop {
            let p1 = read_tty_secret("Enter new LUKS passphrase for /var: ")?;
            if p1.is_empty() {
                eprintln!("Passphrase cannot be empty.");
                continue;
            }
            let p2 = read_tty_secret("Confirm LUKS passphrase for /var: ")?;
            if p1 == p2 {
                write_file_atomic(keyfile_path, p1.as_bytes(), 0o600, false)?;
                return Ok(true);
            }
            eprintln!("Passphrases did not match, try again.");
        },
        LuksMode::Tpm2 | LuksMode::None => Ok(false),
    }
}

fn generate_wg_privkey_openssl() -> Result<String, String> {
    let mut gen = Command::new("openssl")
        .args(["genpkey", "-algorithm", "X25519"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run openssl genpkey: {e}"))?;
    let gen_out = gen
        .stdout
        .take()
        .ok_or_else(|| "failed to capture openssl genpkey stdout".to_string())?;
    let pkey_out = Command::new("openssl")
        .args(["pkey", "-outform", "DER"])
        .stdin(Stdio::from(gen_out))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run openssl pkey: {e}"))?;
    let gen_status = gen
        .wait()
        .map_err(|e| format!("failed waiting for openssl genpkey: {e}"))?;
    if !gen_status.success() || !pkey_out.status.success() {
        return Err("openssl X25519 key generation failed".to_string());
    }
    if pkey_out.stdout.len() < 32 {
        return Err("unexpected X25519 DER length from openssl".to_string());
    }
    let raw_key = &pkey_out.stdout[pkey_out.stdout.len() - 32..];
    Ok(encode_base64(raw_key))
}

fn derive_wg_pubkey_openssl(priv_b64: &str) -> Result<String, String> {
    let raw = decode_base64(priv_b64)?;
    if raw.len() != 32 {
        return Err("WireGuard private key must decode to 32 bytes".to_string());
    }
    let mut pkcs8 = Vec::with_capacity(48);
    pkcs8.extend_from_slice(&X25519_PKCS8_PREFIX);
    pkcs8.extend_from_slice(&raw);

    let mut child = Command::new("openssl")
        .args(["pkey", "-inform", "DER", "-pubout", "-outform", "DER"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run openssl pkey -pubout: {e}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open openssl stdin".to_string())?;
        stdin
            .write_all(&pkcs8)
            .map_err(|e| format!("failed writing DER key to openssl: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("failed waiting for openssl pkey -pubout: {e}"))?;
    if !out.status.success() || out.stdout.len() < 32 {
        return Err("openssl X25519 public key derivation failed".to_string());
    }
    let pub_raw = &out.stdout[out.stdout.len() - 32..];
    Ok(encode_base64(pub_raw))
}

fn generate_wg_privkey() -> Result<String, String> {
    if command_exists("wg") {
        let out = Command::new("wg")
            .arg("genkey")
            .output()
            .map_err(|e| format!("failed to run wg genkey: {e}"))?;
        if !out.status.success() {
            return Err("wg genkey failed".to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        generate_wg_privkey_openssl()
    }
}

fn derive_wg_pubkey(priv_b64: &str) -> Result<String, String> {
    if command_exists("wg") {
        let mut child = Command::new("wg")
            .arg("pubkey")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to run wg pubkey: {e}"))?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| "failed to open wg pubkey stdin".to_string())?;
            writeln!(stdin, "{priv_b64}")
                .map_err(|e| format!("failed writing key to wg pubkey: {e}"))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("failed waiting for wg pubkey: {e}"))?;
        if !out.status.success() {
            return Err("wg pubkey failed".to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        derive_wg_pubkey_openssl(priv_b64)
    }
}

fn run_systemd_creds_target(
    mid_file: &Path,
    secret_file: &Path,
    args: &[&str],
    stdin_bytes: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    if !Path::new("/etc/machine-id").exists() {
        return Err("host /etc/machine-id does not exist for mount bind".to_string());
    }

    let mut cmd = Command::new("unshare");
    cmd.arg("--mount")
        .env("TARGET_MID_FILE", mid_file)
        .env("SYSTEMD_CREDENTIAL_SECRET", secret_file)
        .args([
            "sh",
            "-eu",
            "-c",
            "mount --bind \"$TARGET_MID_FILE\" /etc/machine-id && exec systemd-creds \"$@\"",
            "sh",
        ])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if stdin_bytes.is_some() {
        cmd.stdin(Stdio::piped());
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn unshare/systemd-creds: {e}"))?;

    if let Some(data) = stdin_bytes {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open systemd-creds stdin".to_string())?;
        stdin
            .write_all(data)
            .map_err(|e| format!("failed writing to systemd-creds stdin: {e}"))?;
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("failed waiting for systemd-creds: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "systemd-creds {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

fn generate_random_machine_id() -> Result<String, String> {
    let uuid = fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map_err(|_| "/proc/sys/kernel/random/uuid unreadable; pass --machine-id".to_string())?;
    let id: String = uuid.chars().filter(|c| *c != '-' && !c.is_whitespace()).collect();
    validate_machine_id(&id)?;
    Ok(id)
}

pub fn run(config: Config) -> Result<(), String> {
    if effective_uid()? != 0 {
        return Err("must be run as root".to_string());
    }

    if let Some(ref img) = config.image {
        let meta = fs::metadata(img)
            .map_err(|e| format!("image '{}' cannot be accessed: {e}", img.display()))?;
        if !meta.is_file() {
            return Err(format!("image '{}' is not a regular file", img.display()));
        }
    }

    for key in &config.ssh_keys {
        let meta = fs::metadata(key)
            .map_err(|e| format!("SSH public key '{}' cannot be read: {e}", key.display()))?;
        if meta.len() == 0 {
            return Err(format!("SSH public key '{}' is empty", key.display()));
        }
    }

    if let Some(ref wg_key) = config.wg_key_file {
        let meta = fs::metadata(wg_key).map_err(|e| {
            format!(
                "WireGuard private key '{}' cannot be read: {e}",
                wg_key.display()
            )
        })?;
        if meta.len() == 0 {
            return Err(format!(
                "WireGuard private key '{}' is empty",
                wg_key.display()
            ));
        }
    }

    for cmd in [
        "systemd-repart",
        "lsblk",
        "blkid",
        "mount",
        "umount",
        "udevadm",
        "unshare",
    ] {
        require_command(cmd)?;
    }

    let need_wg = config.wg_address.is_some() || config.wg_key_file.is_some();
    if need_wg {
        require_command("systemd-creds")?;
        if !command_exists("wg") && !command_exists("openssl") {
            return Err("either 'wg' or 'openssl' is required for WireGuard keys".to_string());
        }
    }

    let target_meta = fs::metadata(&config.target).map_err(|e| {
        format!(
            "--target '{}' cannot be accessed: {e}",
            config.target.display()
        )
    })?;
    let is_block = target_meta.file_type().is_block_device();
    let is_file = target_meta.is_file();
    if !is_block && !is_file {
        return Err(format!(
            "--target '{}' is neither a block device nor a regular file",
            config.target.display()
        ));
    }

    // Declare RAII guards in unwind-safe reverse drop order:
    // temp_dir -> loop_guard -> luks_guard -> mount_guard.
    let temp_dir = TempDirGuard::create()?;
    let esp_mnt = temp_dir.path().join("mnt-esp");
    let var_mnt = temp_dir.path().join("mnt-var");
    let repart_dir = temp_dir.path().join("repart.d");
    let luks_keyfile = temp_dir.path().join("luks.key");
    create_dir_mode(&esp_mnt, 0o700)?;
    create_dir_mode(&var_mnt, 0o700)?;
    create_dir_mode(&repart_dir, 0o700)?;

    if let Some(ref img) = config.image {
        flash_image(img, &config.target, is_block, config.assume_yes)?;
    }

    let mut loop_guard = if is_file {
        let lg = LoopGuard::attach(&config.target)?;
        let _ = Command::new("udevadm").arg("settle").status();
        Some(lg)
    } else {
        let _ = Command::new("partprobe").arg(&config.target).status();
        let _ = Command::new("udevadm").arg("settle").status();
        None
    };

    let dev: PathBuf = match &loop_guard {
        Some(lg) => lg
            .device()
            .ok_or_else(|| "loop device missing".to_string())?
            .to_path_buf(),
        None => config.target.clone(),
    };

    let has_keyfile = prepare_luks_keyfile(&config.luks_mode, &luks_keyfile)?;

    for (name, content) in generate_repart_configs(config.root_slot_size_mib, &config.luks_mode) {
        write_file_atomic(&repart_dir.join(name), content.as_bytes(), 0o644, false)?;
    }

    println!("Running systemd-repart on '{}'...", dev.display());
    let mut repart_cmd = Command::new("systemd-repart");
    repart_cmd
        .arg("--dry-run=no")
        .arg(format!("--definitions={}", repart_dir.display()));
    if has_keyfile {
        repart_cmd.arg(format!("--key-file={}", luks_keyfile.display()));
    }
    repart_cmd.arg(&dev);
    let repart_status = repart_cmd
        .status()
        .map_err(|e| format!("failed to run systemd-repart: {e}"))?;
    if !repart_status.success() {
        return Err("systemd-repart failed".to_string());
    }

    let _ = Command::new("partx").arg("-u").arg(&dev).status();
    let _ = Command::new("udevadm").arg("settle").status();

    let lsblk_out = Command::new("lsblk")
        .args(["-nrpo", "TYPE,PATH,PARTTYPE,PARTLABEL"])
        .arg(&dev)
        .output()
        .map_err(|e| format!("failed to run lsblk: {e}"))?;
    if !lsblk_out.status.success() {
        return Err("lsblk failed".to_string());
    }
    let lsblk_text = String::from_utf8_lossy(&lsblk_out.stdout);
    let (esp_part_str, var_part_str) = parse_lsblk_partitions(&lsblk_text);
    let esp_part = PathBuf::from(
        esp_part_str.ok_or_else(|| format!("could not locate ESP partition on '{}'", dev.display()))?,
    );
    let var_part = PathBuf::from(
        var_part_str.ok_or_else(|| format!("could not locate var partition on '{}'", dev.display()))?,
    );

    // Mount ESP and provision initrd unlock authorized_keys
    {
        let mut esp_guard = MountGuard::mount(&esp_part, &esp_mnt)?;
        if !config.ssh_keys.is_empty() {
            let unlock_dir = esp_mnt.join("unlock");
            fs::create_dir_all(&unlock_dir)
                .map_err(|e| format!("failed to create '{}': {e}", unlock_dir.display()))?;
            install_authorized_keys(
                &config.ssh_keys,
                &unlock_dir.join("authorized_keys"),
                0o644,
                true,
            )?;
            println!("Wrote initrd unlock SSH keys to ESP (/unlock/authorized_keys).");
        }
        esp_guard.unmount()?;
    }

    // Open/mount /var partition
    let blkid_out = Command::new("blkid")
        .args(["-o", "value", "-s", "TYPE"])
        .arg(&var_part)
        .output()
        .map_err(|e| format!("failed to run blkid: {e}"))?;
    let var_fstype = String::from_utf8_lossy(&blkid_out.stdout).trim().to_string();

    let mut luks_guard = if var_fstype == "crypto_LUKS" {
        require_command("cryptsetup")?;
        let mapper = format!("iota-provision-var-{}", std::process::id());
        let kf = if has_keyfile {
            Some(luks_keyfile.as_path())
        } else {
            None
        };
        Some(LuksGuard::open(&var_part, mapper, kf)?)
    } else {
        None
    };

    let var_dev: PathBuf = match &luks_guard {
        Some(lg) => lg
            .mapper_path()
            .ok_or_else(|| "LUKS mapper path missing".to_string())?,
        None => var_part,
    };

    let mut var_guard = MountGuard::mount(&var_dev, &var_mnt)?;

    // Provision Tier 1 directory structure with required permissions.
    // Do NOT create $VAR_MNT/.selinux-labeled so Iota's initrd runs setfiles -m -F on first boot.
    for dir in [
        "roothome",
        "roothome/.ssh",
        "lib/credstore.encrypted",
        "lib/ssh",
    ] {
        create_dir_mode(&var_mnt.join(dir), 0o700)?;
    }
    for dir in [
        "lib",
        "lib/iota",
        "lib/iota/site",
        "lib/iota/site/gens",
        "lib/systemd",
    ] {
        create_dir_mode(&var_mnt.join(dir), 0o755)?;
    }

    if !config.ssh_keys.is_empty() {
        install_authorized_keys(
            &config.ssh_keys,
            &var_mnt.join("roothome/.ssh/authorized_keys"),
            0o600,
            false,
        )?;
        println!("Wrote SSH keys to /var/roothome/.ssh/authorized_keys.");
    }

    // Provision target machine-id before systemd-creds (host credential secret binds to /etc/machine-id)
    let target_mid_file = var_mnt.join("lib/iota/machine-id");
    let machine_id = if let Some(ref id) = config.machine_id {
        write_file_atomic(&target_mid_file, format!("{id}\n").as_bytes(), 0o444, false)?;
        id.clone()
    } else if target_mid_file.exists()
        && fs::metadata(&target_mid_file).is_ok_and(|m| m.len() > 0)
    {
        let existing = fs::read_to_string(&target_mid_file)
            .map_err(|e| format!("failed to read existing machine-id: {e}"))?;
        let trimmed = existing.trim().to_string();
        validate_machine_id(&trimmed)?;
        trimmed
    } else {
        let generated = generate_random_machine_id()?;
        write_file_atomic(
            &target_mid_file,
            format!("{generated}\n").as_bytes(),
            0o444,
            false,
        )?;
        generated
    };
    println!("Target machine-id: {machine_id}");

    // Provision encrypted WireGuard credential
    let mut wg_pubkey: Option<String> = None;
    if need_wg {
        let wg_privkey = if let Some(ref key_path) = config.wg_key_file {
            let raw = fs::read_to_string(key_path).map_err(|e| {
                format!("failed to read WireGuard key '{}': {e}", key_path.display())
            })?;
            raw.chars().filter(|c| !c.is_whitespace()).collect::<String>()
        } else {
            generate_wg_privkey()?
        };

        let decoded = decode_base64(&wg_privkey)?;
        if decoded.len() != 32 {
            return Err("invalid WireGuard private key (expected 32-byte base64 key)".to_string());
        }

        let pubkey = derive_wg_pubkey(&wg_privkey)?;
        if pubkey.is_empty() {
            return Err("failed to derive WireGuard public key".to_string());
        }

        let cred_secret = var_mnt.join("lib/systemd/credential.secret");
        let cred_name = "network.wireguard.private.wg0";
        let cred_out = var_mnt.join("lib/credstore.encrypted").join(cred_name);
        let cred_tmp = temp_dir.path().join("wg.cred");

        if !cred_secret.exists() {
            run_systemd_creds_target(&target_mid_file, &cred_secret, &["setup"], None)?;
            fs::set_permissions(&cred_secret, Permissions::from_mode(0o400))
                .map_err(|e| format!("failed to chmod credential.secret: {e}"))?;
        }

        let cred_tmp_str = cred_tmp.to_string_lossy().into_owned();
        if config.creds_tpm2 {
            run_systemd_creds_target(
                &target_mid_file,
                &cred_secret,
                &[
                    "encrypt",
                    "--name=network.wireguard.private.wg0",
                    "--with-key=host+tpm2",
                    "--tpm2-pcrs=",
                    "-",
                    &cred_tmp_str,
                ],
                Some(wg_privkey.as_bytes()),
            )?;
        } else {
            run_systemd_creds_target(
                &target_mid_file,
                &cred_secret,
                &[
                    "encrypt",
                    "--name=network.wireguard.private.wg0",
                    "--with-key=host",
                    "-",
                    &cred_tmp_str,
                ],
                Some(wg_privkey.as_bytes()),
            )?;
        }

        let encrypted_bytes = fs::read(&cred_tmp)
            .map_err(|e| format!("failed to read encrypted credential temp file: {e}"))?;
        write_file_atomic(&cred_out, &encrypted_bytes, 0o400, false)?;
        let _ = fs::remove_file(&cred_tmp);

        let cred_out_str = cred_out.to_string_lossy().into_owned();
        let decrypted = run_systemd_creds_target(
            &target_mid_file,
            &cred_secret,
            &[
                "decrypt",
                "--newline=no",
                "--name=network.wireguard.private.wg0",
                &cred_out_str,
                "-",
            ],
            None,
        )?;
        if decrypted != wg_privkey.as_bytes() {
            return Err("credential decryption self-check failed".to_string());
        }

        println!("Encrypted WireGuard credential written to /var/lib/credstore.encrypted/{cred_name}.");
        wg_pubkey = Some(pubkey);
    }

    // Provision Tier 3 site data
    if config.wg_address.is_some() || config.hostname.is_some() {
        let site_dir = var_mnt.join("lib/iota/site");
        let gens_dir = site_dir.join("gens");
        let mut max_gen: u64 = 0;
        for entry in fs::read_dir(&gens_dir)
            .map_err(|e| format!("failed to read '{}': {e}", gens_dir.display()))?
        {
            let entry = entry.map_err(|e| format!("failed reading dir entry: {e}"))?;
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Ok(n) = name.parse::<u64>() {
                        max_gen = max_gen.max(n);
                    }
                }
            }
        }
        let next_gen = max_gen + 1;
        let next_dir = gens_dir.join(next_gen.to_string());
        create_dir_mode(&next_dir, 0o755)?;

        let current_link = site_dir.join("current");
        if current_link.is_dir() {
            for fname in ["wg0-address.conf", "hostname"] {
                let src = current_link.join(fname);
                if src.is_file() {
                    fs::copy(&src, next_dir.join(fname))
                        .map_err(|e| format!("failed to copy '{fname}' from current gen: {e}"))?;
                }
            }
        }

        if let Some(ref addr) = config.wg_address {
            let content = format!("# Written by iota-site. Per-host wg0 address.\n[Network]\nAddress={addr}\n");
            write_file_atomic(
                &next_dir.join("wg0-address.conf"),
                content.as_bytes(),
                0o644,
                false,
            )?;
        }

        if let Some(ref hn) = config.hostname {
            write_file_atomic(
                &next_dir.join("hostname"),
                format!("{hn}\n").as_bytes(),
                0o644,
                false,
            )?;
        }

        let tmp_link = site_dir.join(format!(".current.tmp.{}", std::process::id()));
        let _ = fs::remove_file(&tmp_link);
        symlink(format!("gens/{next_gen}"), &tmp_link)
            .map_err(|e| format!("failed to create symlink '{}': {e}", tmp_link.display()))?;
        fs::rename(&tmp_link, &current_link)
            .map_err(|e| format!("failed to update '{}': {e}", current_link.display()))?;
        println!("Committed Tier 3 site generation gens/{next_gen} as current.");
    }

    let _ = Command::new("sync").status();
    var_guard.unmount()?;
    if let Some(ref mut lg) = luks_guard {
        lg.close()?;
    }
    if let Some(ref mut lg) = loop_guard {
        lg.detach()?;
    }

    if let Some(pubkey) = wg_pubkey {
        let host_ip = config
            .wg_address
            .as_deref()
            .and_then(|a| a.split_once('/').map(|(ip, _)| ip))
            .unwrap_or("10.100.0.X");
        let node_label = config.hostname.as_deref().unwrap_or("iota-node");
        println!(
            "\nWireGuard public key: {pubkey}\n\n\
             Add this host to mkosi.extra/usr/lib/systemd/network/50-wg0.netdev:\n\n\
             [WireGuardPeer]\n\
             PublicKey={pubkey}\n\
             AllowedIPs={host_ip}/32\n\
             Endpoint=<public address of {node_label}>:51820\n\
             PersistentKeepalive=25"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openssl_x25519_keygen_and_pubkey_roundtrip() {
        if !command_exists("openssl") {
            return;
        }
        let priv_b64 = generate_wg_privkey_openssl().unwrap();
        let priv_bytes = decode_base64(&priv_b64).unwrap();
        assert_eq!(priv_bytes.len(), 32);

        let pub_b64 = derive_wg_pubkey_openssl(&priv_b64).unwrap();
        let pub_bytes = decode_base64(&pub_b64).unwrap();
        assert_eq!(pub_bytes.len(), 32);
        assert_ne!(priv_b64, pub_b64);
    }
}
