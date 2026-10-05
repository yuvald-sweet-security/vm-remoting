//! MCP server exposing a multi-backend remoting dispatcher as Model Context Protocol tools.
//!
//! It runs a command on a configured remote target, driving each target type directly. The
//! guest command is delivered over stdin (wsl/ssh) or an env var (hyperv), never on the
//! command line, so transport-layer quoting can't corrupt it:
//!   * `wsl`    → `wsl.exe -d <distro> [-u <user>] -- bash -ls`   (command on stdin)
//!   * `ssh`    → `ssh [-i key] [-p port] -o BatchMode=yes [-o opt]... <dest> bash -ls` (command on
//!     stdin)
//!   * `hyperv` → `pwsh` running `Invoke-Command -VMName ... [-Credential ...]` (PowerShell Direct
//!     is the only way into a Hyper-V guest, so this one backend needs PowerShell — the dispatch
//!     logic itself lives here; the command travels in via `$env:VM_GUEST_CMD`).
//!   * `fusion` → a child worker using Fusion's `vmrun` and VMware Tools to transfer a
//!     PowerShell script and poll its output files and exit status.
//!
//! Commands can also run as **background jobs**. A foreground call cannot return until the
//! transport's stdout/stderr pipes hit EOF, and any process backgrounded inside the guest
//! inherits those pipes — so `cmd &` returns in the guest while the MCP call still blocks for
//! the job's full lifetime. Backgrounding therefore has to happen on this side of the
//! transport: `run_command` with `background` spawns the transport, hands its pipes to reader
//! tasks and returns a job id immediately, and `job_list` / `job_output` / `job_stop` work
//! against an in-process registry. Jobs are children of this server, so they live as long as
//! it does.
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
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use indexmap::IndexMap;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::oneshot,
};
use tracing_subscriber::EnvFilter;

mod fusion;

/// Foreground timeout applied when a call does not pass one. Overridable with
/// `VM_REMOTING_TIMEOUT_MS`; `0` disables the bound entirely. A hung guest command would
/// otherwise wedge the call forever, since nothing else bounds it.
const DEFAULT_TIMEOUT_MS: u64 = 600_000;

/// How long a *finished* job is kept in the registry before `job_list` prunes it. Overridable
/// with `VM_REMOTING_JOB_TTL_DAYS`; `0` disables pruning. Running jobs are never pruned.
const DEFAULT_JOB_TTL_DAYS: u64 = 7;

/// Lines of stdout/stderr returned per stream by `job_output` when not told otherwise. A
/// long build's log is far larger than a tool result should be, so the tail is the default
/// and the full text is opt-in.
const DEFAULT_TAIL_LINES: u32 = 200;

/// Per-stream cap on captured output, for both foreground calls and jobs. A runaway command
/// can print without bound, and everything it prints is held in memory until the call
/// returns (or, for a job, until the job is pruned).
const MAX_CAPTURE: usize = 8 * 1024 * 1024;

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
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Target {
    Fusion {
        #[serde(flatten)]
        config: fusion::FusionTarget,
    },
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
            Target::Fusion { config } => ("fusion", &config.vmx_path),
            Target::Hyperv { vm_name, .. } => ("hyperv", vm_name),
            Target::Ssh { host, .. } => ("ssh", host),
            Target::Wsl { distro, .. } => ("wsl", distro.as_deref().unwrap_or("(default)")),
        }
    }
}

/// External programs the server drives, so each can be repointed without touching the code.
/// `ssh` in particular is ambiguous on Windows — System32's OpenSSH, Git's bundled copy and
/// a Scoop/Cygwin build can all be on `PATH`, and they differ in config handling and
/// `ProxyCommand` support.
#[derive(Debug, Clone)]
struct Programs {
    worker: String,
    /// PowerShell executable used for `hyperv` targets only. `VM_REMOTING_PWSH`.
    pwsh: String,
    /// SSH client used for `ssh` targets. `VM_REMOTING_SSH`.
    ssh: String,
}

impl Programs {
    fn from_env() -> Self {
        Self {
            worker: env::current_exe()
                .expect("server executable path")
                .to_string_lossy()
                .into_owned(),
            pwsh: env::var("VM_REMOTING_PWSH").unwrap_or_else(|_| "pwsh".to_string()),
            ssh: env::var("VM_REMOTING_SSH").unwrap_or_else(|_| "ssh".to_string()),
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
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
    /// Command to feed to the child via stdin instead of the command line. Used for the
    /// `wsl`/`ssh` backends so the guest command never passes through `wsl.exe`/`ssh.exe`
    /// argument quoting (which mangles single quotes, `$`, etc.). `None` means no stdin.
    stdin: Option<String>,
}

/// Decide how to invoke `command` on `target`, using the executables named by `progs`. This
/// performs no I/O — see [`VmServer::run_on`] for the credential preflight and the actual
/// spawn.
///
/// For `wsl`/`ssh` the guest command is delivered over stdin to `bash -ls` (a login shell
/// reading from stdin) rather than as an argv element. Passing it as an argument means it
/// survives two rounds of quoting (Rust's Windows command-line encoding, then `wsl.exe`/
/// `ssh.exe`'s own parsing), which corrupts quotes — e.g. single-quoted text gets expanded.
/// stdin sidesteps all of that. Hyper-V already avoids the problem via environment variables.
fn plan_command(progs: &Programs, target: &Target, command: &str) -> CommandPlan {
    match target {
        Target::Fusion { config } => CommandPlan {
            program: progs.worker.clone(),
            args: vec!["--fusion-worker".into()],
            env: vec![(
                "VM_FUSION_TARGET".into(),
                Some(serde_json::to_string(config).expect("serializable Fusion target")),
            )],
            stdin: Some(command.to_string()),
        },
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
            args.extend(["--".into(), "bash".into(), "-ls".into()]);
            CommandPlan {
                program: "wsl.exe".into(),
                args,
                env: Vec::new(),
                stdin: Some(command.to_string()),
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
            // Run a login shell that reads the command from the forwarded stdin.
            args.push("bash".into());
            args.push("-ls".into());
            CommandPlan {
                program: progs.ssh.clone(),
                args,
                env: Vec::new(),
                stdin: Some(command.to_string()),
            }
        }
        Target::Hyperv { vm_name, cred_path } => CommandPlan {
            program: progs.pwsh.clone(),
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
            stdin: None,
        },
    }
}

// ---------------------------------------------------------------------------------------
// Background jobs
//
// A foreground call cannot return until the transport's stdout/stderr pipes hit EOF, and a
// process backgrounded inside the guest inherits those pipes — which is why `cmd &` returns
// in the guest while the MCP call still blocks for the job's whole lifetime. Backgrounding
// therefore has to happen on our side of the transport: the server spawns the transport,
// hands its pipes to reader tasks, and returns a job id instead of awaiting it.
//
// Jobs are children of this server process, so they last as long as it does — which is the
// intent, since the targets are local VMs. A job's captured output lives in memory, capped
// at MAX_CAPTURE per stream.
// ---------------------------------------------------------------------------------------

/// Mint a job id. Millisecond timestamp plus a per-process counter — the counter is what
/// guarantees uniqueness, since several jobs can start within the same millisecond. Display
/// order comes from the registry's insertion order, not from the id.
fn new_job_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("j{ms}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// Output captured from one stream of a running command.
#[derive(Debug, Default)]
struct Captured {
    bytes: Vec<u8>,
    /// Bytes discarded from the front once [`MAX_CAPTURE`] was reached. A build log is more
    /// useful from its end than its start, so the cap keeps the tail.
    dropped: u64,
}

impl Captured {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > MAX_CAPTURE {
            let excess = self.bytes.len() - MAX_CAPTURE;
            self.bytes.drain(..excess);
            self.dropped += excess as u64;
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// How a job ended, or that it hasn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobState {
    Running,
    /// The command finished on its own. `None` means it was terminated without an exit code.
    Exited(Option<i32>),
    /// `job_stop` tore the transport down.
    Stopped,
}

impl JobState {
    fn label(&self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Exited(_) => "exited",
            JobState::Stopped => "stopped",
        }
    }
}

/// A background job: the transport process, its captured output, and how to stop it.
struct Job {
    id: String,
    target: String,
    command: String,
    started: SystemTime,
    stdout: Arc<Mutex<Captured>>,
    stderr: Arc<Mutex<Captured>>,
    /// Written by the waiter task once the transport exits.
    outcome: Arc<Mutex<(JobState, Option<SystemTime>)>>,
    /// Fires the waiter task's kill path. Taken on first use — stopping twice is a no-op.
    stop: Mutex<Option<oneshot::Sender<()>>>,
}

impl Job {
    fn state(&self) -> JobState {
        self.outcome
            .lock()
            .map(|o| o.0)
            .unwrap_or(JobState::Running)
    }

    /// How long the job ran, or has been running so far.
    fn elapsed(&self) -> Duration {
        let end = self
            .outcome
            .lock()
            .ok()
            .and_then(|o| o.1)
            .unwrap_or_else(SystemTime::now);
        end.duration_since(self.started).unwrap_or_default()
    }
}

/// Compact, human-scale duration: `45s`, `3m12s`, `1h04m`, `2d03h`.
fn fmt_duration(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86_400,
        (secs % 86_400) / 3_600,
        (secs % 3_600) / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d{h:02}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Keep the last `tail` lines of `text`, reporting `(kept, total)` so the caller can say how
/// much it elided. `tail` of 0 keeps everything.
fn tail_lines(text: &str, tail: u32) -> (String, usize, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if tail == 0 || total <= tail as usize {
        return (text.trim_end_matches('\n').to_string(), total, total);
    }
    let kept = &lines[total - tail as usize..];
    (kept.join("\n"), kept.len(), total)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RunArgs {
    /// The command line to run on the target, written for the target's native shell
    /// (PowerShell for `hyperv`/`fusion` targets, bash for `ssh`/`wsl` targets).
    command: String,
    /// Optional target name (see `list_targets`). Omit to run on the configured active
    /// target — that is the default and preferred for most calls. Set this only when a
    /// specific VM is required.
    #[serde(default)]
    target: Option<String>,
    /// Start the command as a detached background job and return its id immediately instead
    /// of waiting. Use this for anything long-running (builds, test suites, installs):
    /// backgrounding inside the command itself (`cmd &`, `nohup`) does NOT work — the call
    /// still blocks until the job finishes. Poll it with `job_output`.
    #[serde(default)]
    background: bool,
    /// Give up after this many milliseconds and report whatever the command printed so far.
    /// Omit for the server default (10 minutes unless `VM_REMOTING_TIMEOUT_MS` says
    /// otherwise); 0 waits forever. Ignored when `background` is set.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Arguments for the tools that address one existing job.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct JobArgs {
    /// Job id, as returned by `run_command` with `background` or listed by `job_list`.
    job_id: String,
    /// How many lines to return from the end of each of stdout and stderr. Defaults to 200;
    /// 0 returns the entire captured log, which for a long build can be very large.
    #[serde(default)]
    tail_lines: Option<u32>,
}

/// Arguments for `job_list`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct JobListArgs {
    /// Show only jobs started on this target. Omit to list jobs on every target.
    #[serde(default)]
    target: Option<String>,
}

/// Captured result of one transport invocation.
struct RunOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
    /// Set when the call hit its timeout and the transport was killed. `stdout`/`stderr`
    /// then hold whatever had been printed up to that point.
    timed_out: bool,
}

/// Read a `u64` setting from the environment, falling back to `default` if it is unset or
/// unparseable.
fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Copy a child pipe into a shared buffer. This runs as its own task rather than via
/// `wait_with_output` so that the output stays reachable while the command is still going —
/// which is what lets a job be polled, and a timed-out call report what it managed to print.
async fn drain<R: AsyncReadExt + Unpin>(mut src: R, sink: Arc<Mutex<Captured>>) {
    let mut chunk = [0u8; 8192];
    loop {
        match src.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => match sink.lock() {
                Ok(mut buf) => buf.push(&chunk[..n]),
                Err(_) => break,
            },
        }
    }
}

#[derive(Clone)]
struct VmServer {
    programs: Programs,
    targets_file: PathBuf,
    /// Foreground timeout used when a call does not supply one; 0 means unbounded.
    default_timeout_ms: u64,
    /// How long a finished job stays in the registry, in seconds; 0 disables pruning.
    job_ttl_secs: u64,
    /// Background jobs, newest last. Shared across clones of the handler, which is why the
    /// registry is behind an `Arc` while everything else here is plain data.
    jobs: Arc<Mutex<IndexMap<String, Arc<Job>>>>,
    // Consumed by the `#[tool_handler]`-generated routing code; not read directly.
    #[allow(dead_code)]
    tool_router: ToolRouter<VmServer>,
}

#[tool_router]
impl VmServer {
    fn new() -> Self {
        Self {
            programs: Programs::from_env(),
            targets_file: targets_file(),
            default_timeout_ms: env_u64("VM_REMOTING_TIMEOUT_MS", DEFAULT_TIMEOUT_MS),
            job_ttl_secs: env_u64("VM_REMOTING_JOB_TTL_DAYS", DEFAULT_JOB_TTL_DAYS)
                .saturating_mul(86_400),
            jobs: Arc::new(Mutex::new(IndexMap::new())),
            tool_router: Self::tool_router(),
        }
    }

    /// Look a job up by id, with an error that lists what *is* there.
    fn job(&self, id: &str) -> Result<Arc<Job>, McpError> {
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| McpError::internal_error("job registry poisoned", None))?;
        jobs.get(id).cloned().ok_or_else(|| {
            let known: Vec<&str> = jobs.keys().map(String::as_str).collect();
            let known = if known.is_empty() {
                "no jobs have been started in this session".to_string()
            } else {
                format!("known jobs: {}", known.join(", "))
            };
            McpError::invalid_params(format!("unknown job '{id}' ({known})"), None)
        })
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

    /// Spawn the transport for `target` with the guest command already delivered, and start
    /// tasks draining its stdout and stderr into `stdout`/`stderr`.
    ///
    /// Shared by the foreground and background paths: the only difference between them is
    /// who waits for the returned child.
    async fn spawn_transport(
        &self,
        target: &Target,
        command: &str,
        stdout: Arc<Mutex<Captured>>,
        stderr: Arc<Mutex<Captured>>,
    ) -> Result<tokio::process::Child, McpError> {
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

        let plan = plan_command(&self.programs, target, command);
        let mut cmd = Command::new(&plan.program);
        cmd.args(&plan.args);
        for (key, value) in &plan.env {
            match value {
                Some(v) => cmd.env(key, v),
                None => cmd.env_remove(key),
            };
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.stdin(if plan.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        // Without this a transport we stop waiting on — timed out, or stopped — would be
        // left running.
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            McpError::internal_error(format!("failed to launch '{}': {e}", plan.program), None)
        })?;

        // Feed the guest command over stdin (wsl/ssh), then close it so `bash -ls` hits EOF
        // and runs. The payload is tiny, so writing it before draining output can't deadlock.
        if let Some(input) = &plan.stdin {
            let mut child_stdin = child.stdin.take().expect("stdin was piped");
            child_stdin
                .write_all(format!("{input}\n").as_bytes())
                .await
                .map_err(|e| {
                    McpError::internal_error(
                        format!("failed to send command to '{}': {e}", plan.program),
                        None,
                    )
                })?;
            drop(child_stdin);
        }

        tokio::spawn(drain(
            child.stdout.take().expect("stdout was piped"),
            stdout,
        ));
        tokio::spawn(drain(
            child.stderr.take().expect("stderr was piped"),
            stderr,
        ));
        Ok(child)
    }

    /// Run `command` on `target` and wait for it. The native exit code propagates as the
    /// process exit code.
    ///
    /// `timeout_ms` of 0 waits indefinitely. Otherwise the transport process is killed once
    /// the bound elapses and whatever it had printed is returned with `timed_out` set — note
    /// that this tears down the *local* end only, so a guest process may keep running after
    /// a foreground timeout. Background jobs are the better way to run something long.
    async fn run_on(
        &self,
        target: &Target,
        command: &str,
        timeout_ms: u64,
    ) -> Result<RunOutput, McpError> {
        let out_buf = Arc::new(Mutex::new(Captured::default()));
        let err_buf = Arc::new(Mutex::new(Captured::default()));
        let mut child = self
            .spawn_transport(target, command, Arc::clone(&out_buf), Arc::clone(&err_buf))
            .await?;

        let wait_err =
            |e: std::io::Error| McpError::internal_error(format!("transport failed: {e}"), None);
        let mut status = None;
        let mut timed_out = false;
        if timeout_ms == 0 {
            status = Some(child.wait().await.map_err(wait_err)?);
        } else {
            match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
                Ok(st) => status = Some(st.map_err(wait_err)?),
                Err(_) => timed_out = true,
            }
        }
        if timed_out {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }

        // The child has exited, so its pipes are closed and the drains have either finished
        // or are about to; yielding lets them flush the last chunk before we read the buffers.
        tokio::task::yield_now().await;
        let take = |b: &Arc<Mutex<Captured>>| {
            b.lock()
                .map(|mut c| std::mem::take(&mut c.bytes))
                .unwrap_or_default()
        };

        Ok(RunOutput {
            stdout: take(&out_buf),
            stderr: take(&err_buf),
            code: status.and_then(|s| s.code()),
            timed_out,
        })
    }

    /// Start `command` on `target` as a background job and return its id without waiting.
    ///
    /// A waiter task owns the child from here on; it records the outcome when the transport
    /// exits, or kills it if `job_stop` signals first. Giving the waiter sole ownership is
    /// what keeps stopping a job free of any pid handling or platform-specific kill code.
    async fn start_job(
        &self,
        target: &Target,
        target_name: &str,
        command: &str,
    ) -> Result<String, McpError> {
        let stdout = Arc::new(Mutex::new(Captured::default()));
        let stderr = Arc::new(Mutex::new(Captured::default()));
        let child = self
            .spawn_transport(target, command, Arc::clone(&stdout), Arc::clone(&stderr))
            .await?;

        let (stop_tx, stop_rx) = oneshot::channel();
        let outcome = Arc::new(Mutex::new((JobState::Running, None)));
        let job = Arc::new(Job {
            id: new_job_id(),
            target: target_name.to_string(),
            command: command.to_string(),
            started: SystemTime::now(),
            stdout,
            stderr,
            outcome: Arc::clone(&outcome),
            stop: Mutex::new(Some(stop_tx)),
        });

        tokio::spawn(async move {
            let mut child = child;
            let state = tokio::select! {
                status = child.wait() => JobState::Exited(status.ok().and_then(|s| s.code())),
                _ = stop_rx => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    JobState::Stopped
                }
            };
            if let Ok(mut slot) = outcome.lock() {
                *slot = (state, Some(SystemTime::now()));
            }
        });

        let id = job.id.clone();
        self.jobs
            .lock()
            .map_err(|_| McpError::internal_error("job registry poisoned", None))?
            .insert(id.clone(), job);
        Ok(id)
    }

    /// Resolve a tool's optional `target` argument to a named target, falling back to the
    /// configured active one.
    fn resolve<'a>(
        &self,
        cfg: &'a Config,
        requested: Option<&str>,
    ) -> Result<(String, &'a Target), McpError> {
        let name = match requested {
            Some(t) => t.to_string(),
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
        Ok((name, target))
    }

    #[tool(
        description = "List the configured remoting targets (Hyper-V and Fusion VMs, SSH/EC2 hosts, WSL \
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
                       hyperv/fusion targets and on stdin to `bash -ls` (a login shell, so the command \
                       cannot itself read stdin) on ssh/wsl targets. Set `background` for \
                       anything long-running — builds, test suites, installs — to get a job id \
                       back immediately instead of waiting; trying to background inside the \
                       command (`cmd &`, `nohup`) does not work, because the call blocks until \
                       the guest closes the transport's output pipes."
    )]
    async fn run_command(
        &self,
        Parameters(args): Parameters<RunArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.load_config()?;
        let (name, target) = self.resolve(&cfg, args.target.as_deref())?;

        if args.background {
            let id = self.start_job(target, &name, &args.command).await?;
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "{name}$ {}\n\nstarted background job {id}\n\nPoll it with job_output(job_id: \
                 \"{id}\"); stop it with job_stop. Its output is captured as it runs.",
                args.command
            ))]));
        }

        let timeout_ms = args.timeout_ms.unwrap_or(self.default_timeout_ms);
        let out = self.run_on(target, &args.command, timeout_ms).await?;
        Ok(render_output(
            &args.command,
            &name,
            &out.stdout,
            &out.stderr,
            out.code,
            out.timed_out.then_some(timeout_ms),
        ))
    }

    #[tool(
        description = "List background jobs started in this session, newest first, with each \
                       job's target, state (running/exited/stopped), runtime, exit code and \
                       command. Pass `target` to list only one target's jobs. Also drops finished \
                       jobs older than VM_REMOTING_JOB_TTL_DAYS days (7 by default) from the \
                       registry; running jobs are never dropped."
    )]
    async fn job_list(
        &self,
        Parameters(args): Parameters<JobListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| McpError::internal_error("job registry poisoned", None))?;

        let before = jobs.len();
        if self.job_ttl_secs > 0 {
            let ttl = Duration::from_secs(self.job_ttl_secs);
            jobs.retain(|_, j| match j.outcome.lock().ok().and_then(|o| o.1) {
                Some(finished) => finished.elapsed().unwrap_or_default() < ttl,
                None => true,
            });
        }
        let pruned = before - jobs.len();

        let rows: Vec<Arc<Job>> = jobs
            .values()
            .rev()
            .filter(|j| args.target.as_ref().is_none_or(|t| &j.target == t))
            .cloned()
            .collect();
        drop(jobs);

        Ok(CallToolResult::success(vec![Content::text(
            render_job_list(&rows, args.target.as_deref(), pruned),
        )]))
    }

    #[tool(
        description = "Show a background job's current state and the tail of its output. This is \
                       how you poll a job started with run_command(background). Returns the last \
                       200 lines of each of stdout and stderr by default — raise or zero \
                       `tail_lines` for more."
    )]
    async fn job_output(
        &self,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<CallToolResult, McpError> {
        let job = self.job(&args.job_id)?;
        Ok(render_job_detail(
            &job,
            args.tail_lines.unwrap_or(DEFAULT_TAIL_LINES),
        ))
    }

    #[tool(
        description = "Stop watching a background job and tear down its transport process. Output \
                       captured up to that point stays readable with job_output. IMPORTANT: this \
                       kills the local end of the connection, which does NOT reliably kill the \
                       command inside the guest — with no PTY there is no controlling terminal to \
                       deliver SIGHUP, so an ssh/hyperv guest process usually keeps running. If \
                       the guest process itself must die, kill it with run_command (e.g. `pkill \
                       -f <pattern>`) and verify."
    )]
    async fn job_stop(
        &self,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<CallToolResult, McpError> {
        let job = self.job(&args.job_id)?;
        let signalled = job
            .stop
            .lock()
            .ok()
            .and_then(|mut s| s.take())
            .is_some_and(|tx: oneshot::Sender<()>| tx.send(()).is_ok());

        // The waiter task needs a moment to reap the child and record the outcome, so the
        // state we then report is the settled one rather than a stale "running".
        if signalled {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok(CallToolResult::success(vec![Content::text(
            render_job_stop(&job, signalled),
        )]))
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
///
/// `timed_out` carries the bound that was exceeded, if any; the output is then whatever the
/// command printed before it was cut off.
fn render_output(
    command: &str,
    target: &str,
    stdout: &[u8],
    stderr: &[u8],
    code: Option<i32>,
    timed_out: Option<u64>,
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

    let exit = match (timed_out, code) {
        (Some(ms), _) => format!(
            "[timed out after {}; the transport was killed, but a guest process may still be \
             running. Re-run with run_command(background) for anything this long.]",
            fmt_duration(ms / 1_000)
        ),
        (None, Some(c)) => format!("[exit code: {c}]"),
        (None, None) => "[terminated without an exit code]".to_string(),
    };
    // `target$ command` reads like a shell prompt, making the origin of the output clear.
    let body = format!("{target}$ {command}\n\n{out}\n\n{exit}");

    if timed_out.is_none() && matches!(code, Some(0)) {
        CallToolResult::success(vec![Content::text(body)])
    } else {
        CallToolResult::error(vec![Content::text(body)])
    }
}

/// Render `job_list` as an aligned table. `rows` is already newest-first and filtered;
/// `filter` names the target it was filtered to, if any. Pruned jobs are reported in a
/// footer so the drop is never silent.
fn render_job_list(rows: &[Arc<Job>], filter: Option<&str>, pruned: usize) -> String {
    let scope = match filter {
        Some(t) => format!("{t}$ jobs"),
        None => "jobs".to_string(),
    };
    let mut body = format!("{scope}\n\n");

    if rows.is_empty() {
        body.push_str("No background jobs.\n");
    } else {
        let width = |f: &dyn Fn(&Job) -> usize, header: usize| {
            rows.iter().map(|j| f(j)).max().unwrap_or(0).max(header)
        };
        let idw = width(&|j| j.id.len(), 2);
        let tw = width(&|j| j.target.len(), 6);
        body.push_str(&format!(
            "{:<idw$}  {:<tw$}  {:<7}  {:>8}  {:>4}  {}\n",
            "ID", "TARGET", "STATE", "RUNTIME", "EXIT", "COMMAND"
        ));
        for j in rows {
            let state = j.state();
            let exit = match state {
                JobState::Exited(Some(c)) => c.to_string(),
                _ => "-".to_string(),
            };
            body.push_str(&format!(
                "{:<idw$}  {:<tw$}  {:<7}  {:>8}  {:>4}  {}\n",
                j.id,
                j.target,
                state.label(),
                fmt_duration(j.elapsed().as_secs()),
                exit,
                j.command.lines().next().unwrap_or("")
            ));
        }
    }

    if pruned > 0 {
        body.push_str(&format!(
            "\n(dropped {pruned} finished job{} past the retention window)\n",
            if pruned == 1 { "" } else { "s" }
        ));
    }
    body.push_str("\nRead one with job_output(job_id).");
    body
}

/// Render one job's state and captured output, mirroring [`render_output`]'s shape so a
/// finished job reads much like a foreground run. Only a genuinely bad outcome — a non-zero
/// or missing exit code — is flagged as a tool error; a still-running job and a job the
/// caller deliberately stopped are not failures.
fn render_job_detail(job: &Job, tail: u32) -> CallToolResult {
    let state = job.state();
    let elapsed = fmt_duration(job.elapsed().as_secs());
    let mut body = format!(
        "{}$ [job {}] {}\n\n",
        job.target,
        job.id,
        job.command.trim_end().replace('\n', "\n    ")
    );
    body.push_str(&match state {
        JobState::Running => format!("state: running for {elapsed}\n"),
        JobState::Exited(_) => format!("state: exited after {elapsed}\n"),
        JobState::Stopped => format!("state: stopped by job_stop after {elapsed}\n"),
    });

    let section = |name: &str, captured: &Mutex<Captured>| {
        let Ok(c) = captured.lock() else {
            return String::new();
        };
        let text = c.text();
        if text.trim().is_empty() {
            return String::new();
        }
        let (shown, kept, total) = tail_lines(&text, tail);
        let mut head = if kept < total {
            format!("\n[{name}] last {kept} of {total} lines\n")
        } else {
            format!("\n[{name}]\n")
        };
        if c.dropped > 0 {
            head = format!(
                "\n[{name}] last {kept} lines; {} KiB from the start of this stream were \
                 discarded at the capture cap\n",
                c.dropped / 1024
            );
        }
        format!("{head}{shown}\n")
    };
    let out = section("stdout", &job.stdout);
    let err = section("stderr", &job.stderr);
    if out.is_empty() && err.is_empty() {
        body.push_str("\n(no output yet)\n");
    } else {
        body.push_str(&out);
        body.push_str(&err);
    }

    body.push_str(&match state {
        JobState::Exited(Some(c)) => format!("\n[exit code: {c}]"),
        JobState::Exited(None) => "\n[terminated without an exit code]".to_string(),
        JobState::Stopped => "\n[stopped]".to_string(),
        JobState::Running => "\n[still running; poll job_output again for more]".to_string(),
    });

    if matches!(state, JobState::Exited(c) if c != Some(0)) {
        CallToolResult::error(vec![Content::text(body)])
    } else {
        CallToolResult::success(vec![Content::text(body)])
    }
}

/// Render `job_stop`'s reply. `signalled` is false when the job had already finished, so
/// there was nothing left to kill.
fn render_job_stop(job: &Job, signalled: bool) -> String {
    let outcome = if signalled {
        format!(
            "job {} stopped after {}",
            job.id,
            fmt_duration(job.elapsed().as_secs())
        )
    } else {
        match job.state() {
            JobState::Exited(Some(c)) => format!(
                "job {} had already finished on its own (exit code: {c}); nothing to stop",
                job.id
            ),
            _ => format!("job {} was already {}", job.id, job.state().label()),
        }
    };
    let caveat = if signalled {
        " The transport is gone, but the command inside the guest may still be running — kill it \
         with run_command if that matters."
    } else {
        ""
    };
    format!(
        "{}$ job_stop {}\n\n{outcome}\n\nIts output is still readable with job_output.{caveat}",
        job.target, job.id
    )
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
    if env::args().nth(1).as_deref() == Some("--cli") {
        std::process::exit(run_cli(env::args().skip(2).collect()).await?);
    }
    if env::args().nth(1).as_deref() == Some("--fusion-worker") {
        let config: fusion::FusionTarget = serde_json::from_str(&env::var("VM_FUSION_TARGET")?)?;
        let mut command = String::new();
        tokio::io::stdin().read_to_string(&mut command).await?;
        std::process::exit(fusion::run(&config, &command).await?);
    }
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

async fn run_cli(mut args: Vec<String>) -> Result<i32> {
    let server = VmServer::new();
    let mut config = server.load_config().map_err(|e| anyhow::anyhow!("{e}"))?;
    let requested = if args
        .first()
        .is_some_and(|s| s == "--target" || s == "-Target")
    {
        if args.len() < 3 {
            anyhow::bail!("usage: vm.sh --target <name> <command>");
        }
        args.remove(0);
        Some(args.remove(0))
    } else {
        None
    };
    match args.first().map(String::as_str) {
        None => {
            println!(
                "Usage: vm.sh [--target <name>] <command> | list | use <name> | save-cred <name>"
            );
            return Ok(0);
        }
        Some("list") if requested.is_none() => {
            println!("{}", render_list(&config));
            return Ok(0);
        }
        Some("use") if requested.is_none() => {
            if args.len() != 2 {
                anyhow::bail!("usage: vm.sh use <name>");
            }
            server
                .resolve(&config, Some(&args[1]))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            config.current = Some(args[1].clone());
            let parent = server
                .targets_file
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            std::fs::create_dir_all(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            std::io::Write::write_all(
                &mut file,
                serde_json::to_string_pretty(&config)?.as_bytes(),
            )?;
            file.persist(&server.targets_file)?;
            println!("Active target -> {}", args[1]);
            return Ok(0);
        }
        _ => {}
    }
    let (_, target) = server
        .resolve(&config, requested.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let plan = plan_command(&server.programs, target, &args.join(" "));
    let mut transport = Command::new(&plan.program);
    transport
        .args(&plan.args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    for (key, value) in &plan.env {
        match value {
            Some(value) => transport.env(key, value),
            None => transport.env_remove(key),
        };
    }
    transport.stdin(if plan.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = transport
        .spawn()
        .with_context(|| format!("failed to launch '{}'", plan.program))?;
    if let Some(input) = plan.stdin {
        let mut stdin = child.stdin.take().expect("piped command input");
        stdin.write_all(format!("{input}\n").as_bytes()).await?;
    }
    Ok(child.wait().await?.code().unwrap_or(1))
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

    fn progs(pwsh: &str, ssh: &str) -> Programs {
        Programs {
            worker: "vm-remoting-mcp".into(),
            pwsh: pwsh.into(),
            ssh: ssh.into(),
        }
    }

    #[test]
    fn fusion_config_and_plan_keep_command_off_argv() {
        let cfg: Config = serde_json::from_str(r#"{"targets":{"win":{"type":"fusion","vmxPath":"/VMs/Windows dev.vmx","user":"user","password":"guest-secret","vmPassword":"vm-secret"}}}"#).unwrap();
        let target = &cfg.targets["win"];
        assert_eq!(target.summary(), ("fusion", "/VMs/Windows dev.vmx"));
        let command = "Write-Output 'héllo $HOME'; exit 7";
        let plan = plan_command(&progs("pwsh", "ssh"), target, command);
        assert_eq!(args_of(&plan), ["--fusion-worker"]);
        assert_eq!(plan.stdin.as_deref(), Some(command));
        let env = plan.env[0].1.as_ref().unwrap();
        assert!(env.contains("vm-secret"));
        assert!(!env.contains(command));
        let roundtrip: Config =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(roundtrip.targets["win"].summary(), target.summary());
    }

    #[test]
    fn plan_wsl_with_distro_and_user() {
        let t = Target::Wsl {
            distro: Some("Ubuntu-Claude".into()),
            user: Some("dev".into()),
        };
        let plan = plan_command(&progs("pwsh", "ssh"), &t, "uname -a");
        assert_eq!(plan.program, "wsl.exe");
        // Command goes over stdin, not the argv (avoids wsl.exe quote mangling).
        assert_eq!(
            args_of(&plan),
            ["-d", "Ubuntu-Claude", "-u", "dev", "--", "bash", "-ls"]
        );
        assert_eq!(plan.stdin.as_deref(), Some("uname -a"));
        assert!(plan.env.is_empty());
    }

    #[test]
    fn plan_wsl_defaults_to_default_distro() {
        let t = Target::Wsl {
            distro: None,
            user: None,
        };
        let plan = plan_command(&progs("pwsh", "ssh"), &t, "echo hi");
        assert_eq!(args_of(&plan), ["--", "bash", "-ls"]);
        assert_eq!(plan.stdin.as_deref(), Some("echo hi"));
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
        let plan = plan_command(&progs("pwsh", "ssh"), &t, "ls -la");
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
                "bash",
                "-ls"
            ]
        );
        assert_eq!(plan.stdin.as_deref(), Some("ls -la"));
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
        let plan = plan_command(&progs("pwsh", "ssh"), &t, "whoami");
        assert_eq!(
            args_of(&plan),
            ["-o", "BatchMode=yes", "host", "bash", "-ls"]
        );
        assert_eq!(plan.stdin.as_deref(), Some("whoami"));
    }

    #[test]
    fn plan_wsl_command_never_appears_in_argv() {
        // The whole point of the stdin fix: a command with quotes/`$` must not be on the
        // command line where wsl.exe can mangle it.
        let t = Target::Wsl {
            distro: None,
            user: None,
        };
        let tricky = "echo 'literal $HOME' \"$(id -u)\"";
        let plan = plan_command(&progs("pwsh", "ssh"), &t, tricky);
        assert!(
            !plan
                .args
                .iter()
                .any(|a| a.contains("$") || a.contains('\''))
        );
        assert_eq!(plan.stdin.as_deref(), Some(tricky));
    }

    #[test]
    fn plan_hyperv_passes_values_via_env_not_args() {
        let t = Target::Hyperv {
            vm_name: "Win 11".into(),
            cred_path: Some("c.xml".into()),
        };
        let plan = plan_command(&progs("pwsh-7", "ssh"), &t, "whoami");
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
        // Hyper-V passes the command via env, not stdin.
        assert_eq!(plan.stdin, None);
    }

    #[test]
    fn plan_hyperv_without_cred_clears_credpath() {
        let t = Target::Hyperv {
            vm_name: "VM".into(),
            cred_path: None,
        };
        let plan = plan_command(&progs("pwsh", "ssh"), &t, "hostname");
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
        let r = render_output("uname -a", "ubuntu", b"hello\n", b"", Some(0), None);
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
        let r = render_output("do-thing", "winvm", b"out", b"boom", Some(3), None);
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
        let r = render_output("noop", "ubuntu", b"", b"", Some(0), None);
        assert!(result_text(&r).contains("(no output)"));
    }

    #[test]
    fn render_output_no_exit_code_is_error() {
        let r = render_output("crash", "ubuntu", b"", b"", None, None);
        assert!(is_error(&r));
        assert!(result_text(&r).contains("terminated without an exit code"));
    }

    #[test]
    fn render_output_timeout_keeps_partial_output_and_errors() {
        // The point of capturing into buffers rather than `wait_with_output`: a command that
        // is cut off still reports what it printed first.
        let r = render_output("slow", "ubuntu", b"partial\n", b"", None, Some(90_000));
        assert!(is_error(&r));
        let text = result_text(&r);
        assert!(text.contains("partial"));
        assert!(text.contains("timed out after 1m30s"), "got {text:?}");
        assert!(!text.contains("[exit code"));
    }

    #[test]
    fn render_output_timeout_beats_a_zero_exit_code() {
        // A killed transport can still report status 0 on some platforms; the timeout must
        // win, or a cut-off command would be rendered as a clean success.
        let r = render_output("slow", "ubuntu", b"", b"", Some(0), Some(1_000));
        assert!(is_error(&r));
    }

    // ---- background jobs ---------------------------------------------------

    /// A job that has run for exactly `ran_for` seconds. `started`/`finished` are derived
    /// from one another so the rendered duration is exact rather than a hair under.
    fn job_with(state: JobState, ran_for: u64, stdout: &str) -> Job {
        let ran_for = Duration::from_secs(ran_for);
        let started = SystemTime::now() - ran_for;
        let finished = match state {
            JobState::Running => None,
            _ => Some(started + ran_for),
        };
        let mut out = Captured::default();
        out.push(stdout.as_bytes());
        Job {
            id: "j100-0".into(),
            target: "ubuntu".into(),
            command: "cargo build --release".into(),
            started,
            stdout: Arc::new(Mutex::new(out)),
            stderr: Arc::new(Mutex::new(Captured::default())),
            outcome: Arc::new(Mutex::new((state, finished))),
            stop: Mutex::new(None),
        }
    }

    #[test]
    fn job_ids_are_unique_within_a_millisecond() {
        // The timestamp alone is not enough: a burst of starts lands in the same
        // millisecond, so the counter is what keeps ids distinct.
        let ids: Vec<String> = (0..1000).map(|_| new_job_id()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "job ids must not collide");
    }

    #[test]
    fn captured_keeps_the_tail_once_it_hits_the_cap() {
        let mut c = Captured::default();
        c.push(&vec![b'a'; MAX_CAPTURE]);
        c.push(b"the end");
        assert_eq!(c.bytes.len(), MAX_CAPTURE);
        assert_eq!(c.dropped, 7);
        assert!(c.text().ends_with("the end"), "the tail must survive");
    }

    #[test]
    fn tail_lines_trims_to_the_last_n() {
        let text = "1\n2\n3\n4\n5\n";
        assert_eq!(tail_lines(text, 2), ("4\n5".to_string(), 2, 5));
        // Asking for more than there is, or for everything, returns everything.
        assert_eq!(tail_lines(text, 99), ("1\n2\n3\n4\n5".to_string(), 5, 5));
        assert_eq!(tail_lines(text, 0), ("1\n2\n3\n4\n5".to_string(), 5, 5));
        assert_eq!(tail_lines("", 5), (String::new(), 0, 0));
    }

    #[test]
    fn fmt_duration_scales_by_magnitude() {
        assert_eq!(fmt_duration(0), "0s");
        assert_eq!(fmt_duration(45), "45s");
        assert_eq!(fmt_duration(192), "3m12s");
        assert_eq!(fmt_duration(3_840), "1h04m");
        assert_eq!(fmt_duration(183_600), "2d03h");
    }

    #[test]
    fn render_job_detail_running_is_not_an_error() {
        let job = job_with(JobState::Running, 192, "Compiling serde\n");
        let r = render_job_detail(&job, 200);
        assert!(!is_error(&r), "a job still in flight has not failed");
        let text = result_text(&r);
        assert!(text.starts_with("ubuntu$ [job j100-0] cargo build --release"));
        assert!(text.contains("state: running for 3m12s"), "got {text:?}");
        assert!(text.contains("Compiling serde"));
        assert!(text.contains("[still running"));
    }

    #[test]
    fn render_job_detail_reports_exit_code_and_flags_failure() {
        let ok = render_job_detail(&job_with(JobState::Exited(Some(0)), 10, "done\n"), 200);
        assert!(!is_error(&ok));
        assert!(result_text(&ok).contains("[exit code: 0]"));

        let bad = render_job_detail(&job_with(JobState::Exited(Some(101)), 10, "boom\n"), 200);
        assert!(is_error(&bad));
        assert!(result_text(&bad).contains("[exit code: 101]"));
    }

    #[test]
    fn render_job_detail_stopped_is_not_an_error() {
        // The caller asked for the stop, so it is an outcome, not a failure.
        let r = render_job_detail(&job_with(JobState::Stopped, 30, "partial\n"), 200);
        assert!(!is_error(&r));
        let text = result_text(&r);
        assert!(
            text.contains("stopped by job_stop after 30s"),
            "got {text:?}"
        );
        assert!(
            text.contains("partial"),
            "output before the stop must survive"
        );
    }

    #[test]
    fn render_job_detail_notes_elided_lines() {
        let many: String = (1..=500).map(|i| format!("line {i}\n")).collect();
        let text = result_text(&render_job_detail(
            &job_with(JobState::Running, 5, &many),
            3,
        ));
        assert!(text.contains("last 3 of 500 lines"), "got {text:?}");
        assert!(text.contains("line 500") && !text.contains("line 1\n"));
    }

    #[test]
    fn render_job_detail_without_output_says_so() {
        let text = result_text(&render_job_detail(&job_with(JobState::Running, 1, ""), 200));
        assert!(text.contains("(no output yet)"));
    }

    #[test]
    fn render_job_list_tabulates_and_reports_pruning() {
        let jobs = vec![
            Arc::new(job_with(JobState::Running, 192, "")),
            Arc::new(job_with(JobState::Exited(Some(2)), 61, "")),
        ];
        let text = render_job_list(&jobs, None, 3);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.iter().any(|l| l.starts_with("ID")));
        assert!(text.contains("running"), "got {text}");
        assert!(text.contains("3m12s") && text.contains("1m01s"));
        // A finished job shows its code; a running one has none to show.
        assert!(text.contains("exited"));
        // Pruning is reported rather than done silently.
        assert!(text.contains("dropped 3 finished jobs"), "got {text}");
    }

    #[test]
    fn render_job_list_empty_and_filtered() {
        assert!(render_job_list(&[], None, 0).contains("No background jobs."));
        let filtered = render_job_list(&[], Some("winvm"), 0);
        assert!(filtered.starts_with("winvm$ jobs"), "got {filtered}");
    }

    #[test]
    fn render_job_stop_distinguishes_killed_from_already_finished() {
        let killed = render_job_stop(&job_with(JobState::Stopped, 30, ""), true);
        assert!(killed.contains("stopped after 30s"), "got {killed}");

        let already = render_job_stop(&job_with(JobState::Exited(Some(0)), 5, ""), false);
        assert!(
            already.contains("already finished on its own"),
            "got {already}"
        );
        assert!(already.contains("exit code: 0"));
    }

    // ---- program overrides -------------------------------------------------

    #[test]
    fn ssh_program_is_overridable() {
        // Windows has several ssh.exe on PATH; VM_REMOTING_SSH picks one without touching
        // the config.
        let t = Target::Ssh {
            host: "h".into(),
            user: None,
            key: None,
            port: None,
            options: vec![],
        };
        let plan = plan_command(
            &progs("pwsh", r"C:\Windows\System32\OpenSSH\ssh.exe"),
            &t,
            "id",
        );
        assert_eq!(plan.program, r"C:\Windows\System32\OpenSSH\ssh.exe");
    }

    #[test]
    fn wsl_program_is_not_affected_by_the_ssh_override() {
        let t = Target::Wsl {
            distro: None,
            user: None,
        };
        let plan = plan_command(&progs("pwsh", "other-ssh"), &t, "id");
        assert_eq!(plan.program, "wsl.exe");
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
