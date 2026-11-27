use std::fmt;
use std::path::PathBuf;

use crate::validate::{validate_hostname, validate_machine_id, validate_wg_address};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LuksMode {
    Prompt,
    PassphraseFile(PathBuf),
    Tpm2,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub target: PathBuf,
    pub image: Option<PathBuf>,
    pub ssh_keys: Vec<PathBuf>,
    pub luks_mode: LuksMode,
    pub wg_address: Option<String>,
    pub wg_key_file: Option<PathBuf>,
    pub creds_tpm2: bool,
    pub hostname: Option<String>,
    pub machine_id: Option<String>,
    pub root_slot_size_mib: u64,
    pub assume_yes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliOutcome {
    Help,
    Run(Config),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub message: String,
    pub show_usage: bool,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub fn usage() -> &'static str {
    "\
Usage: iota-provision --target <device-or-image> [OPTIONS]

Required:
  --target <path>               Target block device (/dev/nvme0n1, /dev/sda) or raw image

Optional:
  --image <path>                Flash .raw or .raw.zst image to --target first
  --ssh-key <path>              SSH public key file (repeatable); written to both
                                <ESP>/unlock/authorized_keys and <VAR>/roothome/.ssh/authorized_keys
  --luks-passphrase-file <path> File containing /var LUKS passphrase (strips trailing CR/LF)
  --luks-tpm2                   Encrypt /var using target host's TPM2 (runs on target hardware)
  --no-luks                     Leave /var unencrypted
  --wg-address <addr/prefix>    Per-host WireGuard tunnel address (e.g. 10.100.0.42/24)
  --wg-key-file <path>          Existing base64 WireGuard private key (auto-generated if omitted)
  --creds-tpm2                  Bind encrypted systemd credentials to target TPM2 + host key
  --hostname <name>             Per-host hostname stored in Tier 3 site data
  --machine-id <32-hex>         Explicit 32-char lowercase hex /etc/machine-id
  --root-slot-size-mib <MiB>    Slot A/B root partition size in MiB (default: 2048)
  --yes                         Skip destructive confirmation prompt when flashing a block device
  -h, --help                    Show this help message and exit

If none of --luks-passphrase-file, --luks-tpm2, or --no-luks is specified, the
program prompts interactively on /dev/tty for a /var LUKS passphrase."
}

fn next_val(args: &[String], i: &mut usize, flag: &str) -> Result<String, CliError> {
    *i += 1;
    args.get(*i).cloned().ok_or_else(|| CliError {
        message: format!("{flag} requires a value"),
        show_usage: false,
    })
}

pub fn parse_args(args: &[String]) -> Result<CliOutcome, CliError> {
    let mut target: Option<PathBuf> = None;
    let mut image: Option<PathBuf> = None;
    let mut ssh_keys: Vec<PathBuf> = Vec::new();
    let mut luks_mode: Option<LuksMode> = None;
    let mut wg_address: Option<String> = None;
    let mut wg_key_file: Option<PathBuf> = None;
    let mut creds_tpm2 = false;
    let mut hostname: Option<String> = None;
    let mut machine_id: Option<String> = None;
    let mut root_slot_size_mib: u64 = 2048;
    let mut assume_yes = false;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-h" | "--help" => return Ok(CliOutcome::Help),
            "--target" => {
                target = Some(PathBuf::from(next_val(args, &mut i, "--target")?));
            }
            "--image" => {
                image = Some(PathBuf::from(next_val(args, &mut i, "--image")?));
            }
            "--ssh-key" => {
                ssh_keys.push(PathBuf::from(next_val(args, &mut i, "--ssh-key")?));
            }
            "--luks-passphrase-file" => {
                if luks_mode.is_some() {
                    return Err(CliError {
                        message: "LUKS mode flags are mutually exclusive".to_string(),
                        show_usage: false,
                    });
                }
                luks_mode = Some(LuksMode::PassphraseFile(PathBuf::from(next_val(
                    args,
                    &mut i,
                    "--luks-passphrase-file",
                )?)));
            }
            "--luks-tpm2" => {
                if luks_mode.is_some() {
                    return Err(CliError {
                        message: "LUKS mode flags are mutually exclusive".to_string(),
                        show_usage: false,
                    });
                }
                luks_mode = Some(LuksMode::Tpm2);
            }
            "--no-luks" => {
                if luks_mode.is_some() {
                    return Err(CliError {
                        message: "LUKS mode flags are mutually exclusive".to_string(),
                        show_usage: false,
                    });
                }
                luks_mode = Some(LuksMode::None);
            }
            "--wg-address" => {
                wg_address = Some(next_val(args, &mut i, "--wg-address")?);
            }
            "--wg-key-file" => {
                wg_key_file = Some(PathBuf::from(next_val(args, &mut i, "--wg-key-file")?));
            }
            "--creds-tpm2" => {
                creds_tpm2 = true;
            }
            "--hostname" => {
                hostname = Some(next_val(args, &mut i, "--hostname")?);
            }
            "--machine-id" => {
                machine_id = Some(next_val(args, &mut i, "--machine-id")?);
            }
            "--root-slot-size-mib" => {
                let raw = next_val(args, &mut i, "--root-slot-size-mib")?;
                let parsed: u64 = raw.parse().map_err(|_| CliError {
                    message: "--root-slot-size-mib must be a positive integer".to_string(),
                    show_usage: false,
                })?;
                if parsed == 0 {
                    return Err(CliError {
                        message: "--root-slot-size-mib must be a positive integer".to_string(),
                        show_usage: false,
                    });
                }
                root_slot_size_mib = parsed;
            }
            "--yes" => {
                assume_yes = true;
            }
            other => {
                return Err(CliError {
                    message: format!("unknown option '{other}'"),
                    show_usage: true,
                });
            }
        }
        i += 1;
    }

    let target = target.ok_or_else(|| CliError {
        message: "--target is required".to_string(),
        show_usage: true,
    })?;

    if let Some(ref addr) = wg_address {
        validate_wg_address(addr).map_err(|msg| CliError {
            message: msg,
            show_usage: false,
        })?;
    }

    if let Some(ref hn) = hostname {
        validate_hostname(hn).map_err(|msg| CliError {
            message: msg,
            show_usage: false,
        })?;
    }

    if let Some(ref id) = machine_id {
        validate_machine_id(id).map_err(|msg| CliError {
            message: msg,
            show_usage: false,
        })?;
    }

    Ok(CliOutcome::Run(Config {
        target,
        image,
        ssh_keys,
        luks_mode: luks_mode.unwrap_or(LuksMode::Prompt),
        wg_address,
        wg_key_file,
        creds_tpm2,
        hostname,
        machine_id,
        root_slot_size_mib,
        assume_yes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|v| (*v).to_string()).collect()
    }

    #[test]
    fn parse_help() {
        assert_eq!(parse_args(&s(&["--help"])), Ok(CliOutcome::Help));
        assert_eq!(parse_args(&s(&["-h"])), Ok(CliOutcome::Help));
    }

    #[test]
    fn parse_missing_target() {
        let err = parse_args(&s(&[])).unwrap_err();
        assert!(err.message.contains("--target is required"));
        assert!(err.show_usage);
    }

    #[test]
    fn parse_mutually_exclusive_luks_flags() {
        let err = parse_args(&s(&["--target", "/dev/sda", "--luks-tpm2", "--no-luks"])).unwrap_err();
        assert!(err.message.contains("mutually exclusive"));
    }

    #[test]
    fn parse_full_args() {
        let out = parse_args(&s(&[
            "--target",
            "./disk-test.raw",
            "--image",
            "iota.raw.zst",
            "--ssh-key",
            "/tmp/k1.pub",
            "--ssh-key",
            "/tmp/k2.pub",
            "--luks-passphrase-file",
            "/tmp/pass",
            "--wg-address",
            "10.100.0.42/24",
            "--hostname",
            "node-42",
            "--machine-id",
            "0123456789abcdef0123456789abcdef",
            "--root-slot-size-mib",
            "4096",
            "--creds-tpm2",
            "--yes",
        ]))
        .unwrap();

        assert_eq!(
            out,
            CliOutcome::Run(Config {
                target: PathBuf::from("./disk-test.raw"),
                image: Some(PathBuf::from("iota.raw.zst")),
                ssh_keys: vec![PathBuf::from("/tmp/k1.pub"), PathBuf::from("/tmp/k2.pub")],
                luks_mode: LuksMode::PassphraseFile(PathBuf::from("/tmp/pass")),
                wg_address: Some("10.100.0.42/24".to_string()),
                wg_key_file: None,
                creds_tpm2: true,
                hostname: Some("node-42".to_string()),
                machine_id: Some("0123456789abcdef0123456789abcdef".to_string()),
                root_slot_size_mib: 4096,
                assume_yes: true,
            })
        );
    }
}
