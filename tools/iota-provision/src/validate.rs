use std::net::IpAddr;
use std::str::FromStr;

use crate::cli::LuksMode;

const ESP_GPT_TYPE: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";

pub fn validate_wg_address(addr: &str) -> Result<(), String> {
    let (ip_str, prefix_str) = addr
        .split_once('/')
        .ok_or_else(|| format!("--wg-address must include prefix length, got '{addr}'"))?;
    if prefix_str.contains('/') {
        return Err(format!("multiple '/' in --wg-address '{addr}'"));
    }
    if prefix_str.is_empty() || !prefix_str.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid prefix length '{prefix_str}'"));
    }
    let ip = IpAddr::from_str(ip_str)
        .map_err(|_| format!("invalid IP address '{ip_str}' in --wg-address"))?;
    let prefix: u8 = prefix_str
        .parse()
        .map_err(|_| format!("prefix out of range: '{prefix_str}'"))?;
    let max_prefix = match ip {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    if prefix > max_prefix {
        return Err(format!("prefix out of range for IP: {prefix}"));
    }
    Ok(())
}

pub fn validate_hostname(hn: &str) -> Result<(), String> {
    if hn.is_empty() {
        return Err("--hostname cannot be empty".to_string());
    }
    if hn.len() > 253 {
        return Err("--hostname exceeds 253 characters".to_string());
    }
    if hn.starts_with('-') || hn.ends_with('-') || hn.starts_with('.') || hn.ends_with('.') {
        return Err(format!("invalid hostname '{hn}'"));
    }
    if !hn
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(format!("invalid hostname '{hn}'"));
    }
    Ok(())
}

pub fn validate_machine_id(id: &str) -> Result<(), String> {
    if id.len() != 32 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("--machine-id must be 32 lowercase hexadecimal characters".to_string());
    }
    Ok(())
}

pub fn strip_passphrase_endings(raw: &str) -> &str {
    let without_lf = raw.trim_end_matches('\n');
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

fn b64_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let bytes = input.as_bytes();
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return Err("invalid base64 length".to_string());
    }
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    for (idx, chunk) in bytes.chunks_exact(4).enumerate() {
        let is_last = idx + 1 == bytes.len() / 4;
        let a = b64_val(chunk[0]).ok_or_else(|| "invalid base64 character".to_string())?;
        let b = b64_val(chunk[1]).ok_or_else(|| "invalid base64 character".to_string())?;
        let c = if chunk[2] == b'=' {
            if !is_last || chunk[3] != b'=' {
                return Err("invalid base64 padding".to_string());
            }
            None
        } else {
            Some(b64_val(chunk[2]).ok_or_else(|| "invalid base64 character".to_string())?)
        };
        let d = if chunk[3] == b'=' {
            if !is_last {
                return Err("invalid base64 padding".to_string());
            }
            None
        } else {
            Some(b64_val(chunk[3]).ok_or_else(|| "invalid base64 character".to_string())?)
        };

        out.push((a << 2) | (b >> 4));
        if let Some(cv) = c {
            out.push((b << 4) | (cv >> 2));
            if let Some(dv) = d {
                out.push((cv << 6) | dv);
            }
        }
    }
    Ok(out)
}

pub fn parse_lsblk_partitions(output: &str) -> (Option<String>, Option<String>) {
    let mut esp = None;
    let mut var = None;
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let Some(kind) = fields.next() else {
            continue;
        };
        if kind != "part" {
            continue;
        }
        let Some(path) = fields.next() else {
            continue;
        };
        let parttype = fields.next().unwrap_or("");
        let partlabel = fields.next().unwrap_or("");

        if esp.is_none() && (partlabel == "esp" || parttype.eq_ignore_ascii_case(ESP_GPT_TYPE)) {
            esp = Some(path.to_string());
        } else if var.is_none() && partlabel == "var" {
            var = Some(path.to_string());
        }
    }
    (esp, var)
}

pub fn generate_repart_configs(
    root_slot_size_mib: u64,
    luks_mode: &LuksMode,
) -> Vec<(&'static str, String)> {
    let verity_mib = root_slot_size_mib.div_ceil(64).max(16);
    let encrypt_line = match luks_mode {
        LuksMode::Prompt | LuksMode::PassphraseFile(_) => "Encrypt=key-file\n",
        LuksMode::Tpm2 => "Encrypt=tpm2\n",
        LuksMode::None => "",
    };

    vec![
        (
            "00-esp.conf",
            "[Partition]\nType=esp\nSizeMinBytes=512M\nSizeMaxBytes=1G\n".to_string(),
        ),
        (
            "11-root-verity.conf",
            format!(
                "[Partition]\nType=root-verity\nReadOnly=yes\nSizeMinBytes={verity_mib}M\nSizeMaxBytes={verity_mib}M\n"
            ),
        ),
        (
            "12-root.conf",
            format!(
                "[Partition]\nType=root\nReadOnly=yes\nSizeMinBytes={root_slot_size_mib}M\nSizeMaxBytes={root_slot_size_mib}M\n"
            ),
        ),
        (
            "21-root-verity.conf",
            format!(
                "[Partition]\nType=root-verity\nLabel=_empty\nReadOnly=yes\nSizeMinBytes={verity_mib}M\nSizeMaxBytes={verity_mib}M\n"
            ),
        ),
        (
            "22-root.conf",
            format!(
                "[Partition]\nType=root\nLabel=_empty\nReadOnly=yes\nSizeMinBytes={root_slot_size_mib}M\nSizeMaxBytes={root_slot_size_mib}M\n"
            ),
        ),
        (
            "50-var.conf",
            format!(
                "[Partition]\nType=var\nLabel=var\nFormat=xfs\n{encrypt_line}SizeMinBytes=300M\nGrowFileSystem=yes\nMakeDirectories=/home /etc /.etc-work /opt /roothome /mnt /srv\n"
            ),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn wg_address_validation() {
        assert!(validate_wg_address("10.100.0.42/24").is_ok());
        assert!(validate_wg_address("fd00::1/64").is_ok());
        assert!(validate_wg_address("10.100.0.42").is_err());
        assert!(validate_wg_address("10.100.0.42/33").is_err());
        assert!(validate_wg_address("fd00::1/129").is_err());
        assert!(validate_wg_address("10.100.0.999/24").is_err());
    }

    #[test]
    fn hostname_and_machine_id_validation() {
        assert!(validate_hostname("iota-node-1.example").is_ok());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("-bad").is_err());
        assert!(validate_hostname("bad_char").is_err());

        assert!(validate_machine_id("0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_machine_id("0123456789ABCDEF0123456789ABCDEF").is_err());
        assert!(validate_machine_id("too-short").is_err());
    }

    #[test]
    fn base64_decoding() {
        let key_b64 = "OG9LshdiVnqYZFBtmTzINHjJhy5uk4Rw9rZkugK6708=";
        let decoded = decode_base64(key_b64).unwrap();
        assert_eq!(decoded.len(), 32);
        assert!(decode_base64("abc").is_err());
        assert!(decode_base64("ab=d").is_err());
        assert!(decode_base64("@@@@").is_err());
    }

    #[test]
    fn passphrase_endings() {
        assert_eq!(strip_passphrase_endings("secret\r\n"), "secret");
        assert_eq!(strip_passphrase_endings("secret\n"), "secret");
        assert_eq!(strip_passphrase_endings("secret\r"), "secret");
        assert_eq!(strip_passphrase_endings("sec ret \n"), "sec ret ");
    }

    #[test]
    fn lsblk_partition_parsing() {
        let nvme = "\
disk /dev/nvme0n1
part /dev/nvme0n1p1 C12A7328-F81F-11D2-BA4B-00A0C93EC93B EFI\\x20System
part /dev/nvme0n1p2 4f68bce3-e8cd-4db1-96e7-fbcaf984b709 iota_1.0.0
part /dev/nvme0n1p6 4d21b016-b534-45c2-a9fb-5c16e091fd2d var
";
        assert_eq!(
            parse_lsblk_partitions(nvme),
            (
                Some("/dev/nvme0n1p1".to_string()),
                Some("/dev/nvme0n1p6".to_string())
            )
        );

        let loop_dev = "\
loop /dev/loop0
part /dev/loop0p1 c12a7328-f81f-11d2-ba4b-00a0c93ec93b esp
part /dev/loop0p6 4d21b016-b534-45c2-a9fb-5c16e091fd2d var
";
        assert_eq!(
            parse_lsblk_partitions(loop_dev),
            (
                Some("/dev/loop0p1".to_string()),
                Some("/dev/loop0p6".to_string())
            )
        );

        let sda = "\
disk /dev/sda
part /dev/sda1 21686148-6449-6e6f-744e-656564454649 esp
part /dev/sda2 4d21b016-b534-45c2-a9fb-5c16e091fd2d var
";
        assert_eq!(
            parse_lsblk_partitions(sda),
            (Some("/dev/sda1".to_string()), Some("/dev/sda2".to_string()))
        );
    }

    #[test]
    fn repart_generation() {
        let pass = generate_repart_configs(2048, &LuksMode::PassphraseFile(PathBuf::from("/k")));
        assert_eq!(pass.len(), 6);
        assert!(pass[1].1.contains("SizeMinBytes=32M\nSizeMaxBytes=32M\n"));
        assert!(pass[3].1.contains("SizeMinBytes=32M\nSizeMaxBytes=32M\n"));
        assert!(pass[5].1.contains("Encrypt=key-file\n"));

        let tpm2 = generate_repart_configs(4096, &LuksMode::Tpm2);
        assert!(tpm2[1].1.contains("SizeMinBytes=64M\nSizeMaxBytes=64M\n"));
        assert!(tpm2[2].1.contains("SizeMinBytes=4096M"));
        assert!(tpm2[3].1.contains("SizeMinBytes=64M\nSizeMaxBytes=64M\n"));
        assert!(tpm2[5].1.contains("Encrypt=tpm2\n"));

        let small = generate_repart_configs(512, &LuksMode::None);
        assert!(small[1].1.contains("SizeMinBytes=16M\nSizeMaxBytes=16M\n"));
        assert!(small[3].1.contains("SizeMinBytes=16M\nSizeMaxBytes=16M\n"));
        assert!(!small[5].1.contains("Encrypt="));
    }
}
