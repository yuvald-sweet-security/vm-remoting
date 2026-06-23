//! MCP server exposing a multi-backend remoting dispatcher as Model Context Protocol tools.
//!
//! It runs a command on a configured remote target, driving each target type directly:
//!   * `wsl`    → `wsl.exe -d <distro> [-u <user>] -- bash -lc <cmd>`
//!   * `ssh`    → `ssh [-i key] [-p port] -o BatchMode=yes [-o opt]... <dest> <cmd>`
//!   * `hyperv` → `pwsh` running `Invoke-Command -VMName ... [-Credential ...]` (PowerShell Direct
//!     is the only way into a Hyper-V guest, so this one backend needs PowerShell — the dispatch
//!     logic itself lives here).
//!
//! Targets are read from a `.vm-targets.json` file, located via (first match wins):
//!   1. `VM_TARGETS_FILE`            — explicit path to the JSON file
//!   2. `VM_CONFIG_DIR`/.vm-targets.json
//!   3. `./.vm-targets.json`         — current working directory, if present
//!   4. `<OS per-user config dir>/.vm-targets.json`
//!
//! Because nothing is hard-coded to a build location, `cargo install` yields a working
//! server: put the binary on PATH and a `.vm-targets.json` in the OS config dir (or point
//! at one with `VM_TARGETS_FILE`).
//!
//! Only read/run tools are exposed. Switching the active target and storing Hyper-V
//! credentials (an interactive DPAPI prompt) are interactive, human-only operations and are
//! intentionally left out.

use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::Result;
use indexmap::IndexMap;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use tokio::process::Command;
use tracing_subscriber::EnvFilter;

/// Fixed PowerShell program used for Hyper-V targets. All per-call values (VM name, guest
/// command, credential path) are passed via environment variables, never interpolated into
/// the script text, so there is no command-injection surface. Native guest exit codes do
/// not cross PowerShell Direct on their own, so the guest emits a trailing sentinel that we
/// strip here and turn back into the process exit code.
const HYPERV_PS: &str = r#"
$ErrorActionPreference = 'Stop'
if ($PSStyle) { $PSStyle.OutputRendering = 'PlainText' }  # no ANSI codes in captured output
$script:code = 0
$params = @{ VMName = $env:VM_VMNAME }
if ($env:VM_CREDPATH) { $params.Credential = Import-Clixml $env:VM_CREDPATH }
Invoke-Command @params -ScriptBlock {
    param($cmd)
    $global:LASTEXITCODE = 0
    Invoke-Expression $cmd
    "__VMEXIT__:$LASTEXITCODE"
} -ArgumentList $env:VM_GUEST_CMD | ForEach-Object {
    if ($_ -is [string] -and $_ -match '^__VMEXIT__:(\d+)$') { $script:code = [int]$Matches[1] } else { $_ }
}
exit $script:code
"#;

/// A single remoting target, as stored in `.vm-targets.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Target {
    Hyperv {
        #[serde(rename = "vmName")]
        vm_name: String,
        #[serde(rename = "credPath", default)]
        cred_path: Option<String>,
    },
    Ssh {
        host: String,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        options: Vec<String>,
    },
    Wsl {
        #[serde(default)]
        distro: Option<String>,
        #[serde(default)]
        user: Option<String>,
    },
}

impl Target {
    /// `(kind, label)` for the `list_targets` display.
    fn summary(&self) -> (&'static str, &str) {
        match self {
            Target::Hyperv { vm_name, .. } => ("hyperv", vm_name),
            Target::Ssh { host, .. } => ("ssh", host),
            Target::Wsl { distro, .. } => ("wsl", distro.as_deref().unwrap_or("(default)")),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct Config {
    #[serde(default)]
    current: Option<String>,
    #[serde(default)]
    targets: IndexMap<String, Target>,
}

/// Locate the `.vm-targets.json` file (see module docs for precedence).
fn targets_file() -> PathBuf {
    let cwd = PathBuf::from(".vm-targets.json");
    pick_targets_file(
        env::var_os("VM_TARGETS_FILE").map(PathBuf::from),
        env::var_os("VM_CONFIG_DIR").map(PathBuf::from),
        cwd.is_file().then_some(cwd),
        &os_config_dir(),
    )
}

/// Pure precedence logic for [`targets_file`]: explicit file, then config dir, then an
/// existing cwd config, then the OS config dir. Separated so it can be tested without
/// touching the environment or filesystem.
fn pick_targets_file(
    file_env: Option<PathBuf>,
    dir_env: Option<PathBuf>,
    cwd_config: Option<PathBuf>,
    os_dir: &Path,
) -> PathBuf {
    if let Some(f) = file_env {
        return f;
    }
    if let Some(dir) = dir_env {
        return dir.join(".vm-targets.json");
    }
    if let Some(cwd) = cwd_config {
        return cwd;
    }
    os_dir.join(".vm-targets.json")
}

/// OS-standard per-user config directory for this tool (no env overrides — those are
/// handled in [`targets_file`]).
fn os_config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("vm-remoting");
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(xdg).join("vm-remoting");
        }
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(".config").join("vm-remoting");
        }
    }
    env::temp_dir().join("vm-remoting")
}

/// A fully-resolved external command: program, arguments, and environment overrides
/// (`None` means "remove this variable from the child"). Pure data, so the dispatch logic
/// is unit-testable without spawning anything.
#[derive(Debug, PartialEq, Eq)]
struct CommandPlan {
    program: String,
    args: Vec<String>,
    env: Vec<(String, Option<String>)>,
}

/// Decide how to invoke `command` on `target`. `pwsh` is the PowerShell executable used for
/// Hyper-V targets. This performs no I/O — see [`VmServer::run_on`] for the credential
/// preflight and the actual spawn.
fn plan_command(pwsh: &str, target: &Target, command: &str) -> CommandPlan {
    match target {
        Target::Wsl { distro, user } => {
            let mut args = Vec::new();
            if let Some(d) = distro {
                args.push("-d".into());
                args.push(d.clone());
            }
            if let Some(u) = user {
                args.push("-u".into());
                args.push(u.clone());
            }
            args.extend([
                "--".into(),
                "bash".into(),
                "-lc".into(),
                command.to_string(),
            ]);
            CommandPlan {
                program: "wsl.exe".into(),
                args,
                env: Vec::new(),
            }
        }
        Target::Ssh {
            host,
            user,
            key,
            port,
            options,
        } => {
            let mut args = Vec::new();
            if let Some(k) = key {
                args.push("-i".into());
                args.push(k.clone());
            }
            if let Some(p) = port {
                args.push("-p".into());
                args.push(p.to_string());
            }
            args.push("-o".into());
            args.push("BatchMode=yes".into()); // never hang on a password prompt
            for o in options {
                args.push("-o".into());
                args.push(o.clone());
            }
            args.push(match user {
                Some(u) => format!("{u}@{host}"),
                None => host.clone(),
            });
            args.push(command.to_string());
            CommandPlan {
                program: "ssh".into(),
                args,
                env: Vec::new(),
            }
        }
        Target::Hyperv { vm_name, cred_path } => CommandPlan {
            program: pwsh.to_string(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                HYPERV_PS.into(),
            ],
            // Values flow in via the environment so they are never parsed as PowerShell.
            env: vec![
                ("VM_VMNAME".into(), Some(vm_name.clone())),
                ("VM_GUEST_CMD".into(), Some(command.to_string())),
                ("VM_CREDPATH".into(), cred_path.clone()),
            ],
        },
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RunArgs {
    /// The command line to run on the target, written for the target's native shell
    /// (PowerShell for `hyperv` targets, bash for `ssh`/`wsl` targets).
    command: String,
    /// Optional target name (see `list_targets`). Omit to run on the configured active
    /// target — that is the default and preferred for most calls. Set this only when a
    /// specific VM is required.
    #[serde(default)]
    target: Option<String>,
}

#[derive(Clone)]
struct VmServer {
    /// PowerShell executable used for `hyperv` targets only. Overridable with `VM_PWSH`.
    pwsh: String,
    targets_file: PathBuf,
    // Consumed by the `#[tool_handler]`-generated routing code; not read directly.
    #[allow(dead_code)]
    tool_router: ToolRouter<VmServer>,
}

#[tool_router]
impl VmServer {
    fn new() -> Self {
        Self {
            pwsh: env::var("VM_PWSH").unwrap_or_else(|_| "pwsh".to_string()),
            targets_file: targets_file(),
            tool_router: Self::tool_router(),
        }
    }

    /// Read and parse the targets config. A missing file is treated as "no targets".
    fn load_config(&self) -> Result<Config, McpError> {
        match std::fs::read_to_string(&self.targets_file) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| {
                McpError::internal_error(
                    format!("failed to parse {}: {e}", self.targets_file.display()),
                    None,
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(McpError::internal_error(
                format!("failed to read {}: {e}", self.targets_file.display()),
                None,
            )),
        }
    }

    /// Build the transport command for `target`, run it, and capture its output. The native
    /// exit code propagates as the process exit code.
    async fn run_on(
        &self,
        target: &Target,
        command: &str,
    ) -> Result<std::process::Output, McpError> {
        // Hyper-V is unusable non-interactively without its DPAPI credential file; fail with
        // a clear message rather than letting `Invoke-Command` block/error opaquely.
        if let Target::Hyperv {
            cred_path: Some(cp),
            ..
        } = target
            && !Path::new(cp).exists()
        {
            return Err(McpError::internal_error(
                format!(
                    "credential file '{cp}' missing. Create it interactively with `Get-Credential \
                     | Export-Clixml '{cp}'`."
                ),
                None,
            ));
        }

        let plan = plan_command(&self.pwsh, target, command);
        let mut cmd = Command::new(&plan.program);
        cmd.args(&plan.args);
        for (key, value) in &plan.env {
            match value {
                Some(v) => cmd.env(key, v),
                None => cmd.env_remove(key),
            };
        }
        cmd.stdin(Stdio::null());
        cmd.output().await.map_err(|e| {
            McpError::internal_error(format!("failed to launch '{}': {e}", plan.program), None)
        })
    }

    #[tool(
        description = "List the configured remoting targets (Hyper-V VMs, SSH/EC2 hosts, WSL \
                       distros). The active target — used by run_command when no target is given \
                       — is marked with '*'."
    )]
    async fn list_targets(&self) -> Result<CallToolResult, McpError> {
        let cfg = self.load_config()?;
        let text = if cfg.targets.is_empty() {
            format!(
                "No targets configured. Add some to {}.",
                self.targets_file.display()
            )
        } else {
            render_list(&cfg)
        };
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Run a command on a remoting target and return its combined output and exit \
                       code. Defaults to the configured active target; pass `target` only when a \
                       specific VM is required. The command runs as a PowerShell command line on \
                       hyperv targets and via `bash -lc` on ssh/wsl targets."
    )]
    async fn run_command(
        &self,
        Parameters(args): Parameters<RunArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.load_config()?;

        let name = match &args.target {
            Some(t) => t.clone(),
            None => cfg.current.clone().ok_or_else(|| {
                McpError::invalid_params(
                    "no target given and no active target is configured; pass `target` or set a \
                     `current` target in .vm-targets.json",
                    None,
                )
            })?,
        };

        let target = cfg.targets.get(&name).ok_or_else(|| {
            let known = cfg.targets.keys().cloned().collect::<Vec<_>>().join(", ");
            McpError::invalid_params(format!("unknown target '{name}'. Known: {known}"), None)
        })?;

        let out = self.run_on(target, &args.command).await?;
        Ok(render_output(
            &args.command,
            &name,
            &out.stdout,
            &out.stderr,
            out.status.code(),
        ))
    }
}

/// Render the target list; the active target is marked with `*`. Assumes a non-empty set.
fn render_list(cfg: &Config) -> String {
    let mut out = String::new();
    for (name, target) in &cfg.targets {
        let marker = if cfg.current.as_deref() == Some(name) {
            '*'
        } else {
            ' '
        };
        let (kind, label) = target.summary();
        out.push_str(&format!("{marker} {name:<14} {kind:<7} {label}\n"));
    }
    out.trim_end().to_string()
}

/// Turn captured process output into an MCP tool result. The result leads with a header
/// echoing the command and the target it ran on (so the UI shows what produced the output),
/// followed by the combined stdout/stderr and a trailing exit-code line. A non-zero (or
/// absent) exit is surfaced as a tool error so the caller notices failures.
fn render_output(
    command: &str,
    target: &str,
    stdout: &[u8],
    stderr: &[u8],
    code: Option<i32>,
) -> CallToolResult {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);

    let mut out = String::new();
    if !stdout.trim().is_empty() {
        out.push_str(stdout.trim_end_matches('\n'));
    }
    if !stderr.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("[stderr]\n");
        out.push_str(stderr.trim_end_matches('\n'));
    }
    if out.is_empty() {
        out.push_str("(no output)");
    }

    let exit = match code {
        Some(c) => format!("[exit code: {c}]"),
        None => "[terminated without an exit code]".to_string(),
    };
    // `target$ command` reads like a shell prompt, making the origin of the output clear.
    let body = format!("{target}$ {command}\n\n{out}\n\n{exit}");

    if matches!(code, Some(0)) {
        CallToolResult::success(vec![Content::text(body)])
    } else {
        CallToolResult::error(vec![Content::text(body)])
    }
}

#[tool_handler]
impl ServerHandler for VmServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Runs commands on configured remoting targets (Hyper-V VMs, SSH/EC2 hosts, WSL \
                 distros). Use `list_targets` to see them, then `run_command` to run on one — \
                 defaulting to the active target unless a specific VM is required. Switching the \
                 active target and storing Hyper-V credentials are human-only and not exposed.",
            )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // All logging MUST go to stderr — stdout is the JSON-RPC channel for the stdio transport.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("starting vm-remoting MCP server");

    let service = VmServer::new()
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("failed to start server: {e:?}"))?;

    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- config parsing ----------------------------------------------------

    #[test]
    fn parses_all_target_types_in_order() {
        let json = r#"{
            "current": "ubuntu",
            "targets": {
                "winvm":  { "type": "hyperv", "vmName": "Win 11", "credPath": "C:\\creds\\w.xml" },
                "nocred": { "type": "hyperv", "vmName": "Win VHLK" },
                "ec2":    { "type": "ssh", "host": "1.2.3.4", "user": "ubuntu", "key": "k.pem", "port": 2222, "options": ["StrictHostKeyChecking=accept-new"] },
                "bare":   { "type": "ssh", "host": "h" },
                "ubuntu": { "type": "wsl", "distro": "Ubuntu-Claude" },
                "wsldef": { "type": "wsl" }
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.current.as_deref(), Some("ubuntu"));
        let names: Vec<&str> = cfg.targets.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            ["winvm", "nocred", "ec2", "bare", "ubuntu", "wsldef"]
        );

        match &cfg.targets["winvm"] {
            Target::Hyperv { vm_name, cred_path } => {
                assert_eq!(vm_name, "Win 11");
                assert_eq!(cred_path.as_deref(), Some("C:\\creds\\w.xml"));
            }
            other => panic!("expected hyperv, got {other:?}"),
        }
        match &cfg.targets["nocred"] {
            Target::Hyperv { cred_path, .. } => assert!(cred_path.is_none()),
            other => panic!("expected hyperv, got {other:?}"),
        }
        match &cfg.targets["ec2"] {
            Target::Ssh {
                host,
                user,
                key,
                port,
                options,
            } => {
                assert_eq!(host, "1.2.3.4");
                assert_eq!(user.as_deref(), Some("ubuntu"));
                assert_eq!(key.as_deref(), Some("k.pem"));
                assert_eq!(*port, Some(2222));
                assert_eq!(options, &["StrictHostKeyChecking=accept-new"]);
            }
            other => panic!("expected ssh, got {other:?}"),
        }
        match &cfg.targets["bare"] {
            Target::Ssh {
                host,
                user,
                key,
                port,
                options,
            } => {
                assert_eq!(host, "h");
                assert!(user.is_none() && key.is_none() && port.is_none());
                assert!(options.is_empty());
            }
            other => panic!("expected ssh, got {other:?}"),
        }
    }

    #[test]
    fn empty_json_is_default_config() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.current.is_none());
        assert!(cfg.targets.is_empty());
    }

    #[test]
    fn unknown_target_type_is_rejected() {
        let json = r#"{ "targets": { "x": { "type": "telnet", "host": "h" } } }"#;
        assert!(serde_json::from_str::<Config>(json).is_err());
    }

    // ---- summary / list rendering ------------------------------------------

    #[test]
    fn summary_labels_per_type() {
        let hv = Target::Hyperv {
            vm_name: "VM".into(),
            cred_path: None,
        };
        let ssh = Target::Ssh {
            host: "h".into(),
            user: None,
            key: None,
            port: None,
            options: vec![],
        };
        let wsl = Target::Wsl {
            distro: Some("U".into()),
            user: None,
        };
        let wsl_def = Target::Wsl {
            distro: None,
            user: None,
        };
        assert_eq!(hv.summary(), ("hyperv", "VM"));
        assert_eq!(ssh.summary(), ("ssh", "h"));
        assert_eq!(wsl.summary(), ("wsl", "U"));
        assert_eq!(wsl_def.summary(), ("wsl", "(default)"));
    }

    #[test]
    fn render_list_marks_active_and_lists_all() {
        let json = r#"{
            "current": "ubuntu",
            "targets": {
                "winvm":  { "type": "hyperv", "vmName": "Win 11" },
                "ubuntu": { "type": "wsl", "distro": "Ubuntu-Claude" }
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        let listing = render_list(&cfg);
        let lines: Vec<&str> = listing.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("  winvm"), "got {:?}", lines[0]);
        assert!(lines[0].contains("hyperv") && lines[0].contains("Win 11"));
        assert!(lines[1].starts_with("* ubuntu"), "got {:?}", lines[1]);
        assert!(lines[1].contains("wsl") && lines[1].contains("Ubuntu-Claude"));
    }

    // ---- plan_command ------------------------------------------------------

    fn args_of(plan: &CommandPlan) -> Vec<&str> {
        plan.args.iter().map(String::as_str).collect()
    }

    #[test]
    fn plan_wsl_with_distro_and_user() {
        let t = Target::Wsl {
            distro: Some("Ubuntu-Claude".into()),
            user: Some("dev".into()),
        };
        let plan = plan_command("pwsh", &t, "uname -a");
        assert_eq!(plan.program, "wsl.exe");
        assert_eq!(
            args_of(&plan),
            [
                "-d",
                "Ubuntu-Claude",
                "-u",
                "dev",
                "--",
                "bash",
                "-lc",
                "uname -a"
            ]
        );
        assert!(plan.env.is_empty());
    }

    #[test]
    fn plan_wsl_defaults_to_default_distro() {
        let t = Target::Wsl {
            distro: None,
            user: None,
        };
        let plan = plan_command("pwsh", &t, "echo hi");
        assert_eq!(args_of(&plan), ["--", "bash", "-lc", "echo hi"]);
    }

    #[test]
    fn plan_ssh_with_all_options() {
        let t = Target::Ssh {
            host: "1.2.3.4".into(),
            user: Some("ubuntu".into()),
            key: Some("k.pem".into()),
            port: Some(2222),
            options: vec!["StrictHostKeyChecking=accept-new".into()],
        };
        let plan = plan_command("pwsh", &t, "ls -la");
        assert_eq!(plan.program, "ssh");
        assert_eq!(
            args_of(&plan),
            [
                "-i",
                "k.pem",
                "-p",
                "2222",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "ubuntu@1.2.3.4",
                "ls -la"
            ]
        );
    }

    #[test]
    fn plan_ssh_minimal_uses_host_only() {
        let t = Target::Ssh {
            host: "host".into(),
            user: None,
            key: None,
            port: None,
            options: vec![],
        };
        let plan = plan_command("pwsh", &t, "whoami");
        assert_eq!(args_of(&plan), ["-o", "BatchMode=yes", "host", "whoami"]);
    }

    #[test]
    fn plan_hyperv_passes_values_via_env_not_args() {
        let t = Target::Hyperv {
            vm_name: "Win 11".into(),
            cred_path: Some("c.xml".into()),
        };
        let plan = plan_command("pwsh-7", &t, "whoami");
        assert_eq!(plan.program, "pwsh-7");
        assert_eq!(
            args_of(&plan),
            ["-NoProfile", "-NonInteractive", "-Command", HYPERV_PS]
        );
        // The guest command is never an argument — only an env var — so it can't be parsed
        // as PowerShell.
        assert!(!args_of(&plan).contains(&"whoami"));
        assert_eq!(
            plan.env,
            vec![
                ("VM_VMNAME".to_string(), Some("Win 11".to_string())),
                ("VM_GUEST_CMD".to_string(), Some("whoami".to_string())),
                ("VM_CREDPATH".to_string(), Some("c.xml".to_string())),
            ]
        );
    }

    #[test]
    fn plan_hyperv_without_cred_clears_credpath() {
        let t = Target::Hyperv {
            vm_name: "VM".into(),
            cred_path: None,
        };
        let plan = plan_command("pwsh", &t, "hostname");
        assert_eq!(
            plan.env.last(),
            Some(&("VM_CREDPATH".to_string(), None)),
            "VM_CREDPATH must be removed, not left to inherit"
        );
    }

    // ---- render_output -----------------------------------------------------

    fn result_text(r: &CallToolResult) -> String {
        serde_json::to_value(r).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn is_error(r: &CallToolResult) -> bool {
        serde_json::to_value(r).unwrap()["isError"]
            .as_bool()
            .unwrap_or(false)
    }

    #[test]
    fn render_output_success() {
        let r = render_output("uname -a", "ubuntu", b"hello\n", b"", Some(0));
        assert!(!is_error(&r));
        let text = result_text(&r);
        // Header echoes the command and target so the UI shows what produced the output.
        assert!(text.starts_with("ubuntu$ uname -a"), "got {text:?}");
        assert!(text.contains("hello"));
        assert!(text.contains("[exit code: 0]"));
        assert!(!text.contains("[stderr]"));
    }

    #[test]
    fn render_output_nonzero_is_error_with_stderr() {
        let r = render_output("do-thing", "winvm", b"out", b"boom", Some(3));
        assert!(is_error(&r));
        let text = result_text(&r);
        assert!(text.starts_with("winvm$ do-thing"), "got {text:?}");
        assert!(text.contains("out"));
        assert!(text.contains("[stderr]"));
        assert!(text.contains("boom"));
        assert!(text.contains("[exit code: 3]"));
    }

    #[test]
    fn render_output_empty_uses_placeholder() {
        let r = render_output("noop", "ubuntu", b"", b"", Some(0));
        assert!(result_text(&r).contains("(no output)"));
    }

    #[test]
    fn render_output_no_exit_code_is_error() {
        let r = render_output("crash", "ubuntu", b"", b"", None);
        assert!(is_error(&r));
        assert!(result_text(&r).contains("terminated without an exit code"));
    }

    // ---- targets-file precedence -------------------------------------------

    #[test]
    fn pick_prefers_explicit_file_env() {
        let p = pick_targets_file(
            Some("X.json".into()),
            Some("dir".into()),
            Some("cwd.json".into()),
            Path::new("os"),
        );
        assert_eq!(p, PathBuf::from("X.json"));
    }

    #[test]
    fn pick_uses_config_dir_when_no_file_env() {
        let p = pick_targets_file(
            None,
            Some(PathBuf::from("dir")),
            Some("cwd.json".into()),
            Path::new("os"),
        );
        assert_eq!(p, PathBuf::from("dir").join(".vm-targets.json"));
    }

    #[test]
    fn pick_uses_cwd_config_when_present() {
        let cwd = PathBuf::from("cwd").join(".vm-targets.json");
        let p = pick_targets_file(None, None, Some(cwd.clone()), Path::new("os"));
        assert_eq!(p, cwd);
    }

    #[test]
    fn pick_falls_back_to_os_dir() {
        let p = pick_targets_file(None, None, None, Path::new("os"));
        assert_eq!(p, Path::new("os").join(".vm-targets.json"));
    }
}
