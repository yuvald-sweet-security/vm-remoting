use std::{env, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct FusionTarget {
    #[serde(rename = "vmxPath")]
    pub(crate) vmx_path: String,
    user: String,
    password: String,
    #[serde(rename = "vmPassword", default)]
    vm_password: Option<String>,
}

struct Vmrun {
    program: String,
    auth: Vec<String>,
    vmx: String,
}

impl std::fmt::Debug for FusionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FusionTarget")
            .field("vmx_path", &self.vmx_path)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl Vmrun {
    fn new(config: &FusionTarget) -> Result<Self> {
        let mut auth = vec![
            "-T".into(),
            "fusion".into(),
            "-gu".into(),
            config.user.clone(),
            "-gp".into(),
            config.password.clone(),
        ];
        if let Some(password) = &config.vm_password {
            auth.extend(["-vp".into(), password.clone()]);
        }
        Ok(Self {
            program: env::var("VM_REMOTING_VMRUN").unwrap_or_else(|_| {
                "/Applications/VMware Fusion.app/Contents/Library/vmrun".into()
            }),
            auth,
            vmx: config.vmx_path.clone(),
        })
    }

    async fn output(&self, operation: &str, args: &[&str]) -> Result<std::process::Output> {
        Command::new(&self.program)
            .args(&self.auth)
            .arg(operation)
            .arg(&self.vmx)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("failed to launch vmrun for {operation}"))
    }

    async fn call(&self, operation: &str, args: &[&str]) -> Result<String> {
        let output = self.output(operation, args).await?;
        if !output.status.success() {
            // vmrun can echo its arguments on errors; redact both supplied passwords.
            let mut message = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            for pair in self.auth.windows(2) {
                if pair[0] == "-gp" || pair[0] == "-vp" {
                    message = message.replace(&pair[1], "[redacted]");
                }
            }
            bail!("vmrun {operation} failed: {}", message.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let output = self.output("fileExistsInGuest", &[path]).await?;
        let response = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() && response.trim() == "The file does not exist." {
            return Ok(false);
        }
        if output.status.success() && response.trim() == "The file exists." {
            return Ok(true);
        }
        bail!(
            "vmrun could not check for the guest output file (status {})",
            output.status
        )
    }

    async fn copy_from(&self, guest: &str, host: &Path) -> Result<Vec<u8>> {
        self.call("copyFileFromGuestToHost", &[guest, &host.to_string_lossy()])
            .await?;
        Ok(std::fs::read(host)?)
    }
}

fn scripts(command: &str, prefix: &str) -> (String, String) {
    let ps = format!(
        "\u{feff}$ErrorActionPreference = 'Stop'\r\n[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)\r\n$global:LASTEXITCODE = 0\r\n{command}\r\nif (-not $?) {{ if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}; exit 1 }}\r\nexit $LASTEXITCODE\r\n"
    );
    let cmd = format!(
        "@echo off\r\nC:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{prefix}.ps1\" >\"{prefix}.out\" 2>\"{prefix}.err\"\r\necho %errorlevel% >\"{prefix}.status.tmp\"\r\nmove /y \"{prefix}.status.tmp\" \"{prefix}.status\" >nul\r\n"
    );
    (ps, cmd)
}

// The worker is a child process so the existing timeout/job-stop paths can tear down
// vmrun operations without changing how the server manages transports.
pub(crate) async fn run(config: &FusionTarget, command: &str) -> Result<i32> {
    let vm = Vmrun::new(config)?;
    let local = tempfile::tempdir()?;
    let prefix = vm.call("createTempfileInGuest", &[]).await?;
    if !prefix.starts_with("C:\\") || prefix.contains(['"', '\r', '\n', '%', '!']) {
        bail!("vmrun returned an unsupported Windows temporary path");
    }
    let result = execute(&vm, command, &prefix, local.path()).await;
    for suffix in [".ps1", ".cmd", ".out", ".err", ".status.tmp", ".status", ""] {
        let _ = vm
            .call("deleteFileInGuest", &[&format!("{prefix}{suffix}")])
            .await;
    }
    result
}

async fn execute(vm: &Vmrun, command: &str, prefix: &str, local: &Path) -> Result<i32> {
    let (ps, cmd) = scripts(command, prefix);
    for (suffix, content) in [(".ps1", ps), (".cmd", cmd)] {
        let host = local.join(&suffix[1..]);
        std::fs::write(&host, content)?;
        vm.call(
            "copyFileFromHostToGuest",
            &[&host.to_string_lossy(), &format!("{prefix}{suffix}")],
        )
        .await?;
    }
    vm.call(
        "runProgramInGuest",
        &[
            "-noWait",
            "C:\\Windows\\System32\\cmd.exe",
            "/d",
            "/s",
            "/c",
            &format!("\"\"{prefix}.cmd\"\""),
        ],
    )
    .await?;
    let mut offsets = [0usize; 2];
    loop {
        let done = vm.exists(&format!("{prefix}.status")).await?;
        for (i, suffix) in [".out", ".err"].iter().enumerate() {
            let guest = format!("{prefix}{suffix}");
            if done || vm.exists(&guest).await? {
                let bytes = vm.copy_from(&guest, &local.join(&suffix[1..])).await?;
                if bytes.len() < offsets[i] {
                    bail!("Fusion output file shrank while reading");
                }
                let delta = &bytes[offsets[i]..];
                if i == 0 {
                    let mut stream = tokio::io::stdout();
                    stream.write_all(delta).await?;
                    stream.flush().await?;
                } else {
                    let mut stream = tokio::io::stderr();
                    stream.write_all(delta).await?;
                    stream.flush().await?;
                }
                offsets[i] = bytes.len();
            }
        }
        if done {
            let bytes = vm
                .copy_from(&format!("{prefix}.status"), &local.join("status"))
                .await?;
            return String::from_utf8(bytes)?
                .trim()
                .parse()
                .context("invalid Fusion guest exit code");
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_keep_guest_command_literal_and_publish_status_after_exit() {
        let command = "Write-Output 'héllo $HOME'; exit 7";
        let (ps, cmd) = scripts(command, r"C:\Users\user\AppData\Local\Temp\vm a.tmp");
        assert!(ps.starts_with('\u{feff}'));
        assert!(ps.contains(command));
        assert!(!cmd.contains(command));
        assert!(cmd.contains("echo %errorlevel%"));
        assert!(cmd.contains("move /y"));
        assert!(cmd.contains(r#"-File "C:\Users\user\AppData\Local\Temp\vm a.tmp.ps1""#));
    }

    #[test]
    fn inline_passwords_are_preserved_and_debug_is_redacted() {
        let config: FusionTarget = serde_json::from_str(r#"{"vmxPath":"vm.vmx","user":"user","password":" guest-secret ","vmPassword":"vm-secret"}"#).unwrap();
        let vm = Vmrun::new(&config).unwrap();
        assert!(
            vm.auth
                .windows(2)
                .any(|pair| pair == ["-gp", " guest-secret "])
        );
        assert!(vm.auth.windows(2).any(|pair| pair == ["-vp", "vm-secret"]));
        assert!(!format!("{config:?}").contains("secret"));
    }
}
