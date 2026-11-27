use std::fs::{self, DirBuilder, File};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    pub fn create() -> Result<Self, String> {
        let pid = std::process::id();
        for _ in 0..32 {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = PathBuf::from(format!("/tmp/iota-provision.{pid}.{n}"));
            match DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(Self { path: dir }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("failed to create temp dir '{}': {e}", dir.display())),
            }
        }
        Err("failed to allocate unique temporary directory".to_string())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub struct LoopGuard {
    dev: Option<PathBuf>,
}

impl LoopGuard {
    pub fn attach(target: &Path) -> Result<Self, String> {
        let out = Command::new("losetup")
            .args(["--find", "--show", "--partscan"])
            .arg(target)
            .output()
            .map_err(|e| format!("failed to run losetup: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "losetup failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if dev.is_empty() {
            return Err("losetup returned empty device path".to_string());
        }
        Ok(Self {
            dev: Some(PathBuf::from(dev)),
        })
    }

    pub fn device(&self) -> Option<&Path> {
        self.dev.as_deref()
    }

    pub fn detach(&mut self) -> Result<(), String> {
        if let Some(dev) = self.dev.take() {
            let status = Command::new("losetup")
                .arg("-d")
                .arg(&dev)
                .status()
                .map_err(|e| format!("failed to run losetup -d: {e}"))?;
            if !status.success() {
                return Err(format!("losetup -d '{}' failed", dev.display()));
            }
        }
        Ok(())
    }
}

impl Drop for LoopGuard {
    fn drop(&mut self) {
        if let Some(dev) = self.dev.take() {
            let _ = Command::new("losetup").arg("-d").arg(dev).status();
        }
    }
}

pub struct LuksGuard {
    mapper: Option<String>,
}

impl LuksGuard {
    pub fn open(var_part: &Path, mapper_name: String, keyfile: Option<&Path>) -> Result<Self, String> {
        let mut cmd = Command::new("cryptsetup");
        cmd.args(["open", "--allow-discards"]);
        if let Some(kf) = keyfile {
            cmd.arg("--key-file").arg(kf);
        }
        cmd.arg(var_part).arg(&mapper_name);
        let status = cmd
            .status()
            .map_err(|e| format!("failed to run cryptsetup open: {e}"))?;
        if !status.success() {
            return Err(format!("cryptsetup open failed on '{}'", var_part.display()));
        }
        Ok(Self {
            mapper: Some(mapper_name),
        })
    }

    pub fn mapper_path(&self) -> Option<PathBuf> {
        self.mapper
            .as_ref()
            .map(|m| PathBuf::from(format!("/dev/mapper/{m}")))
    }

    pub fn close(&mut self) -> Result<(), String> {
        if let Some(name) = self.mapper.take() {
            if Path::new(&format!("/dev/mapper/{name}")).exists() {
                let status = Command::new("cryptsetup")
                    .args(["close", &name])
                    .status()
                    .map_err(|e| format!("failed to run cryptsetup close: {e}"))?;
                if !status.success() {
                    return Err(format!("cryptsetup close '{name}' failed"));
                }
            }
        }
        Ok(())
    }
}

impl Drop for LuksGuard {
    fn drop(&mut self) {
        if let Some(name) = self.mapper.take() {
            if Path::new(&format!("/dev/mapper/{name}")).exists() {
                let _ = Command::new("cryptsetup").args(["close", &name]).status();
            }
        }
    }
}

pub struct MountGuard {
    target: Option<PathBuf>,
}

impl MountGuard {
    pub fn mount(source: &Path, target: &Path) -> Result<Self, String> {
        let status = Command::new("mount")
            .arg(source)
            .arg(target)
            .status()
            .map_err(|e| format!("failed to run mount: {e}"))?;
        if !status.success() {
            return Err(format!(
                "mount '{}' -> '{}' failed",
                source.display(),
                target.display()
            ));
        }
        Ok(Self {
            target: Some(target.to_path_buf()),
        })
    }

    pub fn unmount(&mut self) -> Result<(), String> {
        if let Some(target) = self.target.take() {
            let status = Command::new("umount")
                .arg(&target)
                .status()
                .map_err(|e| format!("failed to run umount: {e}"))?;
            if !status.success() {
                return Err(format!("umount '{}' failed", target.display()));
            }
        }
        Ok(())
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        if let Some(target) = self.target.take() {
            let _ = Command::new("umount").arg(target).status();
        }
    }
}

pub struct TtyEchoGuard {
    active: bool,
}

impl TtyEchoGuard {
    pub fn disable_echo() -> Result<Self, String> {
        let tty = File::open("/dev/tty")
            .map_err(|e| format!("failed to open /dev/tty for stty: {e}"))?;
        let status = Command::new("stty")
            .arg("-echo")
            .stdin(Stdio::from(tty))
            .status()
            .map_err(|e| format!("failed to run stty -echo: {e}"))?;
        if !status.success() {
            return Err("stty -echo failed".to_string());
        }
        Ok(Self { active: true })
    }
}

impl Drop for TtyEchoGuard {
    fn drop(&mut self) {
        if self.active {
            if let Ok(tty) = File::open("/dev/tty") {
                let _ = Command::new("stty")
                    .arg("echo")
                    .stdin(Stdio::from(tty))
                    .status();
            }
        }
    }
}
