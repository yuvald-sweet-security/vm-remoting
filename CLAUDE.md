# Remoting to a VM / EC2 / WSL — guidance for Claude

This repo provides a stateless remoting dispatcher that runs a command on a configured
remote target (Hyper-V VM, Windows guest in VMware Fusion, SSH host / EC2, or WSL distro) and streams
the output back. Use it for any "run X on the VM / EC2 / WSL" request instead of raw
`Invoke-Command -VMName`, `ssh`, or `wsl`.

There are three front-ends, and they share the same `.vm-targets.json` config so they
interoperate:

1. **`vm-remoting` MCP server** — a native Rust server (`src/main.rs`, binary
   `vm-remoting-mcp`) exposing `list_targets`, `run_command` and the `job_*` tools. **Use
   this by default.**
2. **`vm.ps1`** — the original PowerShell dispatcher. **Fallback only** — use it when the
   `vm-remoting` MCP server is not registered in this session.

**Which to use:** if the MCP tools `mcp__vm-remoting__run_command` /
`mcp__vm-remoting__list_targets` are available, use them. Otherwise fall back to `vm.ps1`.

## Using the `vm-remoting` MCP server (default)

| Tool | Purpose |
|---|---|
| `list_targets` | List configured targets; the active one is marked `*`. (≈ `vm.ps1 list`) |
| `run_command` | Run a command on a target; returns the combined output + exit code. Set `background` to start a job instead. |
| `job_list` | List background jobs (id, target, state, runtime, exit code, command). |
| `job_output` | Poll one job: its state plus the tail of its output. |
| `job_stop` | Stop watching a job and tear down its transport. |

`run_command` parameters:

- `command` (required) — the command line, written for the target's **native shell**:
  PowerShell on `hyperv`/`fusion` targets, `bash` on `ssh`/`wsl` targets. It is delivered on stdin to
  `bash -ls` (a login shell), so **the command cannot itself read stdin** — anything
  interactive must be fed from a file or a heredoc inside the command.
- `target` (optional) — target name (see `list_targets`). **Omit it to run on the active
  target; that is the default and what you should do for most calls.** Pass it only when the
  request needs a *specific* VM — then the call is self-contained and race-free.
- `background` (optional) — start a job and return immediately; see below.
- `timeout_ms` (optional) — give up after this long and report the partial output. Defaults
  to 10 minutes; `0` waits forever. Ignored when `background` is set.

Behavior to rely on:

- **Output**: combined stdout + stderr, followed by an `[exit code: N]` line. A non-zero or
  missing exit code is surfaced as a tool *error*, so failures are visible.
- **Exit codes on `hyperv` targets: a PowerShell `exit N` in the command is swallowed.** The
  command runs inside `Invoke-Command -ScriptBlock`, so `exit` unwinds the script block
  without setting the reported status. Native-process exit codes *do* propagate via
  `$LASTEXITCODE`:

  | command on a `hyperv` target | reported |
  |---|---|
  | `"before"; exit 7` | `[exit code: 0]` — wrong |
  | `Write-Error "an error"; exit 7` | `[exit code: 1]` — from the error, not the `exit` |
  | `cmd /c "exit 9"` | `[exit code: 9]` — correct |

  This matters because a PowerShell build/test script that signals failure with `exit 1`
  reports **success**. Don't trust the exit code of a `.ps1` you invoke on a `hyperv`
  target — have the command echo a sentinel (`if (-not $ok) { "FAILED" }`) and check the
  output, or end it with an explicit `cmd /c "exit $code"`. `bash` targets are unaffected:
  `exit 42` there is reported faithfully.
- **Active target**: the `current` pointer in `.vm-targets.json` is global shared state set
  by the human. You can't (and shouldn't) change it through the MCP server — the `use`
  subcommand is deliberately not exposed. Just omit `target` to run on it.
- **Concurrency**: parallel calls against the same or different targets are safe — each call
  opens its own fresh session/connection.
- **Hyper-V credentials**: if a `hyperv` target's DPAPI credential file is missing,
  `run_command` returns an error telling the human to run `vm.ps1 save-cred <name>`
  (interactive — human-only; see [Hyper-V credentials](#hyper-v-credentials) below).

The interactive `use` and `save-cred` subcommands are intentionally **not** exposed by the
MCP server; they remain human-only via `vm.ps1`.

### Background jobs

**Use `run_command` with `background: true` for anything long-running** — builds, test
suites, installs. It returns a job id straight away; poll it with `job_output(job_id)`.

Do **not** try to background inside the command itself. `cmd &`, `nohup`, `disown` and
friends *do not work here*: the call blocks until the transport's output pipes close, and a
process backgrounded in the guest inherits those pipes, so the call still waits for the whole
job. Redirecting the job's output helps but is not enough over SSH. `background: true` is the
only way to express "start this and return".

```
run_command(command: "cargo build --release", background: true)   -> started background job j1787819910549-0
job_output(job_id: "j1787819910549-0")                            -> state + tail of the output so far
```

- `job_output` returns the last 200 lines of each of stdout and stderr; pass `tail_lines` to
  widen that, or `tail_lines: 0` for the whole log.
- A job's state is `running`, `exited` or `stopped`. A non-zero exit is a tool *error*, the
  same as for a foreground call; a still-running or deliberately stopped job is not.
- `job_list` takes an optional `target` to filter; omit it to see every job.
- Jobs are children of the MCP server process, so they last as long as the session does and
  are not visible to other sessions or to `vm.ps1`.
- **A foreground `timeout_ms` orphans the guest process; `job_stop` is the safer of the two.**
  Both tear down the local end of the transport, but they don't behave the same in the guest:
  - `timeout_ms` **leaks**. Observed on an `ssh` target: `sleep 25; touch /tmp/marker` run
    with `timeout_ms: 3000` still created the marker 25s later — the command ran to
    completion after the call had already returned "timed out". So a timed-out call is *not*
    a cancelled call.
  - `job_stop` on the same target **did** take the guest command down (no process left after
    stopping a `for i in $(seq 1 60); do ...; sleep 1; done` loop). Its tool description
    warns the opposite; treat that warning as the worst case, not the norm.

  Neither is guaranteed — with no PTY there is no controlling terminal to deliver SIGHUP, so
  the outcome depends on the transport and on whether the command holds the output pipes. If
  the guest process actually has to die, kill it explicitly (`pkill -f ...`, `Stop-Process`)
  with `run_command` and verify. Prefer `background: true` + `job_stop` over a short
  `timeout_ms` for anything whose side effects you'd have to undo.

## Fallback: `vm.ps1` (only when the MCP server is not registered)

`vm.ps1` is the stateless PowerShell dispatcher the MCP server reimplements. Use it only if
the `vm-remoting` MCP tools are unavailable.

**Default to the bare command** (`vm.ps1 '<cmd>'`), which runs on the active target. Use
it unless you know the request needs a *specific* VM — then pass `-Target <name>` so the
call is self-contained and race-free.

Do **not** call `vm.ps1 use <name>` yourself to switch the active target before running a
command. The active-target pointer (`current` in `.vm-targets.json`) is global shared
state; a programmatic `use` can race with other callers, silently running your command on
the wrong target. Reading the active target with a bare command is fine — the human set
it; *changing* it is what's unsafe. Reserve `use` for interactive human convenience only.

### How to invoke it

Use the **PowerShell tool** and invoke the script by its bare absolute path — **do NOT use
the call operator `&`**:

```
C:\path\to\vm.ps1 'hostname'                 # active target — the default
C:\path\to\vm.ps1 -Target winvm 'hostname'   # only when a specific VM is required
```

(`C:\path\to\vm.ps1` is wherever `vm.ps1` lives on this machine — see the global config.)

Why no `&`: the permission engine parses the PowerShell AST and matches on the command
name. A leading `& ` defeats wildcard/prefix matching, so `PowerShell(& ...vm.ps1 *)`
won't auto-approve and you get a prompt every time. Invoking the bare path lets the rule
for vm.ps1's absolute path (`PowerShell(C:\\path\\to\\vm.ps1 *)`) match with any arguments.

- Run the script as a **single statement** — no trailing `; echo ...` etc. The engine
  splits compound commands on `;` `|` `&&` `||` and requires every segment to be allowed,
  so an appended statement re-triggers the prompt. To get the exit code, run the script
  alone and read `$LASTEXITCODE` on a separate (also-allowed or trivial) line if needed.
- Wrap the guest command in single quotes.
- The guest command runs as a PowerShell command line on `hyperv` targets, via `bash -lc` on
  `wsl` targets, and as an argument to `ssh` (so the remote login shell parses it) on `ssh`
  targets — write it for the target's native shell. Note this differs from the MCP server,
  which sends the command on stdin to `bash -ls` for both `wsl` and `ssh`.
- `vm.ps1` has no equivalent of the MCP server's background jobs — it always waits.
- Fallback if the `PowerShell` tool is unavailable (only `Bash` present): invoke via
  `pwsh -NoProfile -File C:/path/to/vm.ps1 -Target <name> '<cmd>'` and allow
  `Bash(pwsh -NoProfile -File C:/path/to/vm.ps1 *)`.

### Subcommands

| Command | Purpose |
|---|---|
| `vm.ps1 list` | List targets; `*` marks the active one. |
| `vm.ps1 '<cmd>'` | Run on the active target. **Default — use unless a specific VM is required.** |
| `vm.ps1 -Target <name> '<cmd>'` | Run on a specific target. Use when the request needs a particular VM, or for race-free concurrency. |
| `vm.ps1 use <name>` | Set active target (human convenience; don't rely on it programmatically). |
| `vm.ps1 save-cred <name>` | Store Hyper-V guest credentials. **Interactive — the user must run this**, not me. |

### Behavior to rely on

- **Output** streams straight through (stdout + stderr).
- **Exit codes** propagate: the guest's exit code becomes `$LASTEXITCODE` / the process
  exit code. Check it to know if a command succeeded.
- **Concurrency:** running commands in parallel against the same or different targets is
  safe — each call opens its own fresh session/connection. Config writes are atomic. The
  only unsafe pattern is *switching* the active target with `use` and relying on it; if
  concurrent callers might need different targets, pass `-Target` on each (see above).

## Hyper-V credentials

`hyperv` targets need a DPAPI credential file (`Get-Credential | Export-Clixml`),
decryptable only by the same Windows user + machine that created it. This applies to both
front-ends — they read the same file (stored under `%APPDATA%\vm-remoting\.vm-creds\`). If a
target reports a missing credential file, ask the user to run (it prompts interactively,
which my non-interactive shell can't satisfy):

```
& C:\path\to\vm.ps1 save-cred <name>
```

## Bash fallback and Fusion targets

`vm.sh` is the Bash fallback when MCP is unavailable. It shares the config and uses the
Rust binary (`VM_REMOTING_MCP`, PATH, or a local build):

```bash
/absolute/path/to/vm.sh list
/absolute/path/to/vm.sh --target fusion-win 'hostname; whoami'
```

`vm.sh use` and `save-cred` remain human-only operations. Fusion targets run Windows
PowerShell through VMware Tools using Fusion's `vmrun`. The VM must be running with Tools
installed. Put passwords directly in the config; omit `vmPassword` for unencrypted VMs:

```json
"fusion-win": { "type": "fusion", "vmxPath": "/path/Windows.vmwarevm/Windows.vmx", "user": "user", "password": "GUEST-PASSWORD", "vmPassword": "VM-ENCRYPTION-PASSWORD" }
```

`VM_REMOTING_VMRUN` overrides the Fusion
executable path. Explicit PowerShell exits and native exit codes propagate. Output files
are copied back during execution. Timeout/job-stop can leave the guest command running
and temporary files behind; normal completion cleans them up.

## Adding a target

Edit the shared config (default `%APPDATA%\vm-remoting\.vm-targets.json`; used by both the
MCP server and `vm.ps1`). Shapes:

```json
"name": { "type": "hyperv", "vmName": "...", "credPath": "<absolute path printed by save-cred>" }
"name": { "type": "ssh",    "host": "...", "user": "...", "key": "C:\\path\\to\\name.pem", "port": 22, "options": ["StrictHostKeyChecking=accept-new"] }
"name": { "type": "wsl",    "distro": "Ubuntu", "user": "..." }
```

For an EC2 box without a public IP, configure an SSM `ProxyCommand` in the user's ssh
config and use a plain `ssh` target — no separate transport needed.
