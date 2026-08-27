# vm-remoting

Run a command on a configured remote target — a **Hyper-V VM** (via PowerShell Direct), an
**SSH host / EC2 box**, or a **WSL distro** — and stream the output back. Targets live in a
single `.vm-targets.json`, selected by name.

Two front-ends share that config:

| Front-end | What it is | Use it for |
|---|---|---|
| **`vm-remoting-mcp`** | A native Rust [MCP](https://modelcontextprotocol.io) server (`src/main.rs`) exposing `list_targets`, `run_command` and background-job tools. | Agents / Claude Code. |
| **`vm.ps1`** | The original stateless PowerShell dispatcher. | Humans at a terminal. |

The MCP server is a self-contained reimplementation — it does **not** shell out to `vm.ps1`.
It drives `wsl.exe` and `ssh` directly and uses `pwsh` only for Hyper-V (PowerShell Direct
is the only way into a Hyper-V guest). Both front-ends read the same config, so they
interoperate.

## Build & test

```sh
cargo build --release      # binary at target/release/vm-remoting-mcp[.exe]
cargo test                 # unit tests for config parsing, dispatch, rendering
cargo clippy --all-targets
```

## Install

```sh
cargo install --path .                       # from this checkout
# or, from a remote:
cargo install --git <repo-url> vm-remoting-mcp
```

This drops `vm-remoting-mcp` into `~/.cargo/bin` (on `PATH`). The binary is fully
self-contained — no repo checkout or `vm.ps1` needed at runtime.

> `publish = false` in `Cargo.toml` blocks `cargo publish` (crates.io); `--path` and
> `--git` installs work regardless. Remove that line if you want to publish.

## Configure targets

The server reads the same `.vm-targets.json` as `vm.ps1`. It is located by, first match wins:

1. `VM_TARGETS_FILE` — explicit path to the JSON file
2. `VM_CONFIG_DIR`/`.vm-targets.json`
3. `./.vm-targets.json` — the current working directory, if present
4. `<OS per-user config dir>/.vm-targets.json` — the default
   (`%APPDATA%\vm-remoting\` on Windows, `$XDG_CONFIG_HOME`/`~/.config/vm-remoting/` elsewhere)

So after `cargo install`, the zero-config home for your targets is
`%APPDATA%\vm-remoting\.vm-targets.json`. Format (see `.vm-targets.json.example`):

```json
{
  "current": "ubuntu",
  "targets": {
    "winvm":  { "type": "hyperv", "vmName": "Win 11", "credPath": "C:\\Users\\you\\AppData\\Roaming\\vm-remoting\\.vm-creds\\winvm.xml" },
    "ec2":    { "type": "ssh", "host": "1.2.3.4", "user": "ubuntu", "key": "C:\\path\\to\\ec2.pem", "port": 22, "options": ["StrictHostKeyChecking=accept-new"] },
    "ubuntu": { "type": "wsl", "distro": "Ubuntu" }
  }
}
```

`current` is the active target, used by `run_command` when no `target` is given. Switching
it (`vm.ps1 use <name>`) and storing Hyper-V credentials (`vm.ps1 save-cred <name>`, an
interactive DPAPI prompt) are **human-only** — the MCP server does not expose them.

### Hyper-V credentials

A `hyperv` target needs a DPAPI credential file, decryptable only by the same Windows user +
machine that created it, and required for non-interactive use:

```powershell
.\vm.ps1 save-cred winvm   # prompts, writes the .xml to %APPDATA%\vm-remoting\.vm-creds, prints the path
```

Then set `"credPath"` to the absolute path it prints (Windows does not expand `%APPDATA%`
inside the JSON, so paste the literal path). If it's missing, `run_command` returns a clear error.

## Register with Claude Code

A project-scoped [`.mcp.json`](.mcp.json) is included and is intentionally generic:

```json
{ "mcpServers": { "vm-remoting": { "command": "vm-remoting-mcp" } } }
```

It just needs `vm-remoting-mcp` on `PATH` (which `cargo install` arranges). Or register it
yourself:

```sh
claude mcp add vm-remoting -- vm-remoting-mcp
```

The tools then appear as `mcp__vm-remoting__list_targets`, `mcp__vm-remoting__run_command`,
`mcp__vm-remoting__job_list`, `mcp__vm-remoting__job_output` and
`mcp__vm-remoting__job_stop`.

## Background jobs

A foreground call can't return until the transport's stdout/stderr pipes reach EOF, and a
process backgrounded *inside the guest* inherits those pipes. So `cmd &` returns in the guest
while the call keeps blocking for the job's whole lifetime — backgrounding has to happen on
this side of the transport, which is what `background` does:

```jsonc
run_command { "command": "cargo build --release", "background": true }
// -> started background job j1787819910549-0
job_output  { "job_id": "j1787819910549-0" }
// -> state: running for 3m12s  +  the tail of stdout/stderr so far
```

| Tool | Purpose |
|---|---|
| `job_list` | Every job (or one target's), with state, runtime, exit code and command. |
| `job_output` | One job's state plus the last `tail_lines` (default 200; `0` for all) of each stream. |
| `job_stop` | Stop watching a job and tear down its transport. |

Jobs are children of the server process: they live as long as the MCP session, are private to
it, and their output is captured in memory (capped at 8 MiB per stream, keeping the tail).
Finished jobs are dropped from the registry once they age past the retention window.

> **`job_stop` does not reliably kill the guest process.** It kills the local end of the
> transport; because no PTY is allocated there is no controlling terminal to deliver SIGHUP,
> so an `ssh`/`hyperv` guest command generally keeps running. The same applies when a
> foreground call hits `timeout_ms`. To be sure, kill the process in the guest explicitly
> (`pkill -f ...`, `Stop-Process`) and verify.

### Environment overrides

| Variable | Effect |
|---|---|
| `VM_TARGETS_FILE` | Use this exact config file. Shared with `vm.ps1`. |
| `VM_CONFIG_DIR` | Look for `.vm-targets.json` in this directory. Shared with `vm.ps1`. |
| `VM_REMOTING_PWSH` | PowerShell executable for Hyper-V targets (default `pwsh`). |
| `VM_REMOTING_SSH` | SSH client for `ssh` targets (default `ssh`). Useful on Windows, where System32's OpenSSH, Git's bundled copy and a Scoop/Cygwin build can all be on `PATH` and behave differently. |
| `VM_REMOTING_TIMEOUT_MS` | Default foreground timeout in ms (default `600000`); `0` waits forever. |
| `VM_REMOTING_JOB_TTL_DAYS` | How long finished jobs stay in the registry (default `7`); `0` keeps them for the session. |
| `RUST_LOG` | Log filter; logs go to **stderr** (stdout is the JSON-RPC channel). |

### Windows: hosts that sanitize the environment

Some MCP hosts do not hand the server the user's environment. Claude Code in the desktop
app launches it with a sanitized allowlist of roughly sixteen variables — `APPDATA`,
`HOMEDRIVE`, `HOMEPATH`, `LOCALAPPDATA`, `LOGONSERVER`, `PATH`,
`PROCESSOR_ARCHITECTURE`, `PROGRAMFILES`, `SYSTEMDRIVE`, `SYSTEMROOT`, `TEMP`,
`USERDOMAIN`, `USERNAME`, `USERPROFILE`, `WINDIR` — plus whatever the host's own config
sets. Everything else is dropped, including variables that Windows tooling reads
implicitly. Children of this server inherit that reduced environment, so both transports
break, each in a way that points nowhere near the real cause:

| Missing variable | Target type | Symptom |
|---|---|---|
| `ProgramData` | `ssh` | `ssh` exits **255 with no output at all**, even under `-vvv`. Win32-OpenSSH resolves its `__PROGRAMDATA__` system config (`%ProgramData%\ssh\ssh_config`) from this variable and dies before it can report anything. |
| `COMPUTERNAME` | `hyperv` | `Get-VM: Value cannot be null. (Parameter 'name')`. `Get-VM` defaults `-ComputerName` to `$env:COMPUTERNAME`; the null `name` is the **computer** name, not the VM name. `VM_VMNAME` arrives intact. |

Neither symptom implicates the environment, and the Hyper-V one actively misdirects — it
reads as a bad or missing `vmName` in `.vm-targets.json`. Confirm before you go hunting:
if `list_targets` prints the right `vmName` but `run_command` reports a null `name`, the
config is fine and the environment is not.

The fix is to set the variables explicitly in the host's per-server `env` block. For
Claude Code / Claude Desktop:

```json
"vm-remoting": {
  "type": "stdio",
  "command": "vm-remoting-mcp",
  "args": [],
  "env": {
    "ProgramData": "C:\\ProgramData",
    "ALLUSERSPROFILE": "C:\\ProgramData",
    "COMPUTERNAME": "YOUR-HOSTNAME",
    "VM_REMOTING_SSH": "C:\\Windows\\System32\\OpenSSH\\ssh.exe"
  }
}
```

Hardcode literal values here. `${VAR}` expansion in host config is resolved against the
host's environment, which is the one already missing these variables.

Two traps when verifying a fix. The server is spawned once at session start, so an edit
needs a restart to take effect — and a restart can leave the previous
`vm-remoting-mcp.exe` processes alive and still serving calls, so check with
`Get-Process vm-remoting-mcp` and quit the host fully if several are listed. Procmon is
the fastest way to see the truth: it shows the child's full command line, working
directory and environment block, which settles in one capture what the exit codes cannot.
