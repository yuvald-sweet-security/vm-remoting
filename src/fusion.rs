use std::{env, future::Future, path::Path, time::Duration};

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
    #[serde(default)]
    elevated: bool,
    #[serde(default)]
    interactive: bool,
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

fn scripts(command: &str, prefix: &str, elevated: bool) -> (String, String) {
    let preflight = if elevated {
        "$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())\r\nif (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Fusion command did not receive an administrator token' }\r\n"
    } else {
        ""
    };
    let ps = format!(
        "\u{feff}$ErrorActionPreference = 'Stop'\r\n[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)\r\n{preflight}$global:LASTEXITCODE = 0\r\n{command}\r\nif (-not $?) {{ if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}; exit 1 }}\r\nexit $LASTEXITCODE\r\n"
    );
    let cmd = format!(
        "@echo off\r\nC:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{prefix}.ps1\" >\"{prefix}.out\" 2>\"{prefix}.err\"\r\necho %errorlevel% >\"{prefix}.status.tmp\"\r\nmove /y \"{prefix}.status.tmp\" \"{prefix}.status\" >nul\r\n"
    );
    (ps, cmd)
}

fn elevation_launcher(prefix: &str) -> String {
    let prefix = prefix.replace('\'', "''");
    format!(
        r#"$ErrorActionPreference = 'Stop'
try {{
    $process = Start-Process -FilePath "$env:WINDIR\System32\cmd.exe" -Verb RunAs -WindowStyle Hidden -ArgumentList '/d /s /c ""{prefix}.cmd""' -Wait -PassThru
    if ($process.ExitCode -ne 0) {{ throw "Elevated command launcher exited with code $($process.ExitCode)" }}
}} catch {{
    [IO.File]::WriteAllText('{prefix}.err', "Fusion elevation failed: $($_.Exception.Message)", (New-Object Text.UTF8Encoding($false)))
    [IO.File]::WriteAllText('{prefix}.status.tmp', '1')
    Move-Item -LiteralPath '{prefix}.status.tmp' -Destination '{prefix}.status' -Force
}}
"#
    )
}

// The worker is a child process so the existing timeout/job-stop paths can tear down
// vmrun operations without changing how the server manages transports.
pub(crate) async fn run(config: &FusionTarget, command: &str) -> Result<i32> {
    let vm = Vmrun::new(config)?;
    run_with_vm(&vm, config, command).await
}

async fn run_with_vm(vm: &Vmrun, config: &FusionTarget, command: &str) -> Result<i32> {
    let local = tempfile::tempdir()?;
    let prefix = vm.call("createTempfileInGuest", &[]).await?;
    if !prefix.starts_with("C:\\") || prefix.contains(['"', '\r', '\n', '%', '!']) {
        bail!("vmrun returned an unsupported Windows temporary path");
    }
    let result = execute(vm, command, &prefix, local.path(), config).await;
    let code = result.with_context(|| {
        format!("Fusion transport failed; the guest command may still be running. Guest files preserved at {prefix}.*")
    })?;
    for suffix in [
        ".ps1",
        ".cmd",
        ".launcher.ps1",
        ".out",
        ".err",
        ".status.tmp",
        ".status",
        "",
    ] {
        let _ = vm
            .call("deleteFileInGuest", &[&format!("{prefix}{suffix}")])
            .await;
    }
    Ok(code)
}

async fn retry_guest_io<T, F, Fut>(mut operation: F, delay: Duration) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    for attempt in 0..5 {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt == 4 => return Err(error),
            Err(_) => tokio::time::sleep(delay).await,
        }
    }
    unreachable!()
}

async fn execute(
    vm: &Vmrun,
    command: &str,
    prefix: &str,
    local: &Path,
    config: &FusionTarget,
) -> Result<i32> {
    let elevated = config.elevated;
    let (ps, cmd) = scripts(command, prefix, elevated);
    let mut files = vec![(".ps1", ps), (".cmd", cmd)];
    if elevated {
        files.push((".launcher.ps1", elevation_launcher(prefix)));
    }
    for (suffix, content) in files {
        let host = local.join(&suffix[1..]);
        std::fs::write(&host, content)?;
        let host = host.to_string_lossy();
        let guest = format!("{prefix}{suffix}");
        retry_guest_io(
            || async { vm.call("copyFileFromHostToGuest", &[&host, &guest]).await },
            Duration::from_secs(1),
        )
        .await?;
    }
    if elevated {
        vm.call(
            "runProgramInGuest",
            &[
                "-noWait",
                "-interactive",
                "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &format!("{prefix}.launcher.ps1"),
            ],
        )
        .await?;
    } else {
        let mut args = vec!["-noWait"];
        if config.interactive {
            args.push("-interactive");
        }
        let guest_command = format!("\"\"{prefix}.cmd\"\"");
        args.extend([
            "C:\\Windows\\System32\\cmd.exe",
            "/d",
            "/s",
            "/c",
            &guest_command,
        ]);
        vm.call("runProgramInGuest", &args).await?;
    }
    let mut offsets = [0usize; 2];
    loop {
        let status = format!("{prefix}.status");
        let done = retry_guest_io(|| vm.exists(&status), Duration::from_secs(1)).await?;
        for (i, suffix) in [".out", ".err"].iter().enumerate() {
            let guest = format!("{prefix}{suffix}");
            if retry_guest_io(|| vm.exists(&guest), Duration::from_secs(1)).await? {
                let host = local.join(&suffix[1..]);
                let bytes =
                    retry_guest_io(|| vm.copy_from(&guest, &host), Duration::from_secs(1)).await?;
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
            let host = local.join("status");
            let bytes =
                retry_guest_io(|| vm.copy_from(&status, &host), Duration::from_secs(1)).await?;
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

    #[cfg(unix)]
    #[tokio::test]
    async fn monitoring_failure_preserves_guest_files() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("vmrun");
        std::fs::write(
            &program,
            r#"#!/bin/sh
case "$1" in
    createTempfileInGuest) printf '%s\n' 'C:\Temp\test' ;;
    fileExistsInGuest) echo 'The file exists.' ;;
    copyFileFromGuestToHost) echo 'Error: Unknown error' >&2; exit 1 ;;
    deleteFileInGuest) touch "$2/deleted" ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let vm = Vmrun {
            program: program.to_string_lossy().into_owned(),
            auth: vec![],
            vmx: directory.path().to_string_lossy().into_owned(),
        };
        let config: FusionTarget =
            serde_json::from_str(r#"{"vmxPath":"vm.vmx","user":"user","password":"test"}"#)
                .unwrap();
        let error = run_with_vm(&vm, &config, "Write-Output test")
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(r"Guest files preserved at C:\Temp\test.*")
        );
        assert!(!directory.path().join("deleted").exists());
    }

    #[tokio::test]
    async fn guest_reads_recover_from_transient_errors() {
        let mut attempts = 0;
        let value = retry_guest_io(
            || {
                attempts += 1;
                let attempt = attempts;
                async move {
                    if attempt < 3 {
                        bail!("Unknown error");
                    }
                    Ok(b"complete output".to_vec())
                }
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(value, b"complete output");
    }

    #[tokio::test]
    async fn guest_reads_stop_after_repeated_failures() {
        let mut attempts = 0;
        let result: Result<()> = retry_guest_io(
            || {
                attempts += 1;
                async { bail!("persistent copy failure") }
            },
            Duration::ZERO,
        )
        .await;
        assert_eq!(attempts, 5);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("persistent copy failure")
        );
    }

    #[test]
    fn scripts_keep_guest_command_literal_and_publish_status_after_exit() {
        let command = "Write-Output 'héllo $HOME'; exit 7";
        let (ps, cmd) = scripts(command, r"C:\Users\user\AppData\Local\Temp\vm a.tmp", false);
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
        assert!(!config.elevated);
        assert!(!config.interactive);
    }

    #[test]
    fn elevation_checks_token_and_publishes_launch_errors() {
        let (ps, _) = scripts("Write-Output test", r"C:\Temp\test", true);
        assert!(ps.contains("WindowsBuiltInRole]::Administrator"));
        assert!(ps.find("administrator token").unwrap() < ps.find("Write-Output test").unwrap());
        let launcher = elevation_launcher(r"C:\Users\O'Brien\Temp\test");
        assert!(launcher.contains("-Verb RunAs"));
        assert!(launcher.contains("O''Brien"));
        assert!(launcher.contains(".status.tmp', '1'"));
        assert!(launcher.contains("Move-Item -LiteralPath"));
    }
}
