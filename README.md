# vm-remoting

Run a command on a configured remote target — a **Hyper-V VM** (via PowerShell Direct), an
**SSH host / EC2 box**, or a **WSL distro** — and stream the output back. Targets live in a
single `.vm-targets.json`, selected by name.

Two front-ends share that config:

| Front-end | What it is | Use it for |
|---|---|---|
| **`vm-remoting-mcp`** | A native Rust [MCP](https://modelcontextprotocol.io) server (`src/main.rs`) exposing `list_targets` and `run_command` tools. | Agents / Claude Code. |
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
    "winvm":  { "type": "hyperv", "vmName": "Win 11", "credPath": "D:\\path\\to\\winvm.xml" },
    "ec2":    { "type": "ssh", "host": "1.2.3.4", "user": "ubuntu", "key": "D:\\keys\\ec2.pem", "port": 22, "options": ["StrictHostKeyChecking=accept-new"] },
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
& D:\claude-remoting\vm.ps1 save-cred winvm   # prompts, writes the .xml, prints the path
```

Then set `"credPath"` on the target. If it's missing, `run_command` returns a clear error.

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

The tools then appear as `mcp__vm-remoting__list_targets` and `mcp__vm-remoting__run_command`.

### Environment overrides

| Variable | Effect |
|---|---|
| `VM_TARGETS_FILE` | Use this exact config file. |
| `VM_CONFIG_DIR` | Look for `.vm-targets.json` in this directory. |
| `VM_PWSH` | PowerShell executable for Hyper-V targets (default `pwsh`). |
| `RUST_LOG` | Log filter; logs go to **stderr** (stdout is the JSON-RPC channel). |
