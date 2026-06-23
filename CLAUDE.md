# Using `vm.ps1` — guidance for Claude

`vm.ps1` is a stateless dispatcher that runs a command on a configured remote target
(Hyper-V VM via PowerShell Direct, SSH host / EC2, or WSL distro) and streams the output
back. Use it for any "run X on the VM / EC2 / WSL" request instead of raw
`Invoke-Command -VMName`, `ssh`, or `wsl`.

**Default to the bare command** (`vm.ps1 '<cmd>'`), which runs on the active target. Use
it unless you know the request needs a *specific* VM — then pass `-Target <name>` so the
call is self-contained and race-free.

Do **not** call `vm.ps1 use <name>` yourself to switch the active target before running a
command. The active-target pointer (`current` in `.vm-targets.json`) is global shared
state; a programmatic `use` can race with other callers, silently running your command on
the wrong target. Reading the active target with a bare command is fine — the human set
it; *changing* it is what's unsafe. Reserve `use` for interactive human convenience only.

## How to invoke it

Use the **PowerShell tool** and invoke the script by its bare absolute path — **do NOT use
the call operator `&`**:

```
D:\claude-remoting\vm.ps1 'hostname'                 # active target — the default
D:\claude-remoting\vm.ps1 -Target winvm 'hostname'   # only when a specific VM is required
```

Why no `&`: the permission engine parses the PowerShell AST and matches on the command
name. A leading `& ` defeats wildcard/prefix matching, so `PowerShell(& ...vm.ps1 *)`
won't auto-approve and you get a prompt every time. Invoking the bare path lets the rule
`PowerShell(D:\\claude-remoting\\vm.ps1 *)` match with any arguments.

- Run the script as a **single statement** — no trailing `; echo ...` etc. The engine
  splits compound commands on `;` `|` `&&` `||` and requires every segment to be allowed,
  so an appended statement re-triggers the prompt. To get the exit code, run the script
  alone and read `$LASTEXITCODE` on a separate (also-allowed or trivial) line if needed.
- Wrap the guest command in single quotes.
- The guest command runs as a PowerShell command line on `hyperv` targets, and via
  `bash -lc` on `wsl`/`ssh` targets — write it for the target's native shell.
- Fallback if the `PowerShell` tool is unavailable (only `Bash` present): invoke via
  `pwsh -NoProfile -File D:/claude-remoting/vm.ps1 -Target <name> '<cmd>'` and allow
  `Bash(pwsh -NoProfile -File D:/claude-remoting/vm.ps1 *)`.

## Subcommands

| Command | Purpose |
|---|---|
| `vm.ps1 list` | List targets; `*` marks the active one. |
| `vm.ps1 '<cmd>'` | Run on the active target. **Default — use unless a specific VM is required.** |
| `vm.ps1 -Target <name> '<cmd>'` | Run on a specific target. Use when the request needs a particular VM, or for race-free concurrency. |
| `vm.ps1 use <name>` | Set active target (human convenience; don't rely on it programmatically). |
| `vm.ps1 save-cred <name>` | Store Hyper-V guest credentials. **Interactive — the user must run this**, not me. |

## Behavior to rely on

- **Output** streams straight through (stdout + stderr).
- **Exit codes** propagate: the guest's exit code becomes `$LASTEXITCODE` / the process
  exit code. Check it to know if a command succeeded.
- **Concurrency:** running commands in parallel against the same or different targets is
  safe — each call opens its own fresh session/connection. Config writes are atomic. The
  only unsafe pattern is *switching* the active target with `use` and relying on it; if
  concurrent callers might need different targets, pass `-Target` on each (see above).

## Hyper-V credentials

`hyperv` targets need a DPAPI credential file (`Get-Credential | Export-Clixml`),
decryptable only by the same Windows user + machine that created it. If a target reports a
missing credential file, ask the user to run (it prompts interactively, which my
non-interactive shell can't satisfy):

```
& D:\claude-remoting\vm.ps1 save-cred <name>
```

## Adding a target

Edit `D:\claude-remoting\.vm-targets.json`. Shapes:

```json
"name": { "type": "hyperv", "vmName": "...", "credPath": "D:\\claude-remoting\\.vm-creds\\name.xml" }
"name": { "type": "ssh",    "host": "...", "user": "...", "key": "D:\\keys\\name.pem", "port": 22, "options": ["StrictHostKeyChecking=accept-new"] }
"name": { "type": "wsl",    "distro": "Ubuntu", "user": "..." }
```

For an EC2 box without a public IP, configure an SSM `ProxyCommand` in the user's ssh
config and use a plain `ssh` target — no separate transport needed.
