# Remoting to a VM / EC2 / WSL — guidance for Claude

This repo provides a stateless remoting dispatcher that runs a command on a configured
remote target (Hyper-V VM via PowerShell Direct, SSH host / EC2, or WSL distro) and streams
the output back. Use it for any "run X on the VM / EC2 / WSL" request instead of raw
`Invoke-Command -VMName`, `ssh`, or `wsl`.

There are two front-ends, and they share the same `.vm-targets.json` config so they
interoperate:

1. **`vm-remoting` MCP server** — a native Rust server (`src/main.rs`, binary
   `vm-remoting-mcp`) exposing `list_targets` and `run_command` tools. **Use this by
   default.**
2. **`vm.ps1`** — the original PowerShell dispatcher. **Fallback only** — use it when the
   `vm-remoting` MCP server is not registered in this session.

**Which to use:** if the MCP tools `mcp__vm-remoting__run_command` /
`mcp__vm-remoting__list_targets` are available, use them. Otherwise fall back to `vm.ps1`.

## Using the `vm-remoting` MCP server (default)

| Tool | Purpose |
|---|---|
| `list_targets` | List configured targets; the active one is marked `*`. (≈ `vm.ps1 list`) |
| `run_command` | Run a command on a target; returns the combined output + exit code. |

`run_command` parameters:

- `command` (required) — the command line, written for the target's **native shell**:
  PowerShell on `hyperv` targets, `bash -lc` on `ssh`/`wsl` targets.
- `target` (optional) — target name (see `list_targets`). **Omit it to run on the active
  target; that is the default and what you should do for most calls.** Pass it only when the
  request needs a *specific* VM — then the call is self-contained and race-free.

Behavior to rely on:

- **Output**: combined stdout + stderr, followed by an `[exit code: N]` line. A non-zero or
  missing exit code is surfaced as a tool *error*, so failures are visible.
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
- The guest command runs as a PowerShell command line on `hyperv` targets, and via
  `bash -lc` on `wsl`/`ssh` targets — write it for the target's native shell.
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
