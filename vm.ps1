#requires -Version 7.0
<#
.SYNOPSIS
  Run commands seamlessly on a configured target (Hyper-V VM, SSH host / EC2, or WSL distro).

.DESCRIPTION
  A thin, stateless dispatcher. Targets are defined in a JSON config and selected by name.
  Each call opens whatever native transport the target uses, runs the command, and returns
  its output and exit code. No persistent session is required (so it works fine even though
  shell state does not survive between separate invocations).

  Config file (default: <script dir>\.vm-targets.json):
    {
      "current": "winvm",
      "targets": {
        "winvm":  { "type": "hyperv", "vmName": "Win11Dev", "credPath": "D:\\claude-remoting\\.vm-creds\\winvm.xml" },
        "myec2":  { "type": "ssh",    "host": "1.2.3.4", "user": "ubuntu", "key": "D:\\keys\\ec2.pem" },
        "ubuntu": { "type": "wsl",    "distro": "Ubuntu" }
      }
    }

.EXAMPLE
  vm.ps1 list
  vm.ps1 use myec2
  vm.ps1 whoami
  vm.ps1 -Target ubuntu 'uname -a'
  vm.ps1 save-cred winvm        # prompt for & store Hyper-V credentials (DPAPI, current user)
#>
[CmdletBinding(PositionalBinding = $false)]
param(
    [string]$Target,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Rest
)

$ErrorActionPreference = 'Stop'

# OS-standard per-user config directory (mirrors the vm-remoting MCP server).
function Get-OsConfigDir {
    if ($IsWindows) {
        if ($env:APPDATA) { return (Join-Path $env:APPDATA 'vm-remoting') }
    } else {
        if ($env:XDG_CONFIG_HOME) { return (Join-Path $env:XDG_CONFIG_HOME 'vm-remoting') }
        if ($env:HOME)            { return (Join-Path $env:HOME '.config/vm-remoting') }
    }
    Join-Path ([System.IO.Path]::GetTempPath()) 'vm-remoting'
}

# Locate .vm-targets.json the same way the MCP server does, so both front-ends share one
# config (first match wins): explicit file, then config dir, then the current directory,
# then the OS per-user config dir (%APPDATA%\vm-remoting on Windows).
function Resolve-ConfigPath {
    if ($env:VM_TARGETS_FILE) { return $env:VM_TARGETS_FILE }
    if ($env:VM_CONFIG_DIR)   { return (Join-Path $env:VM_CONFIG_DIR '.vm-targets.json') }
    $cwd = Join-Path (Get-Location).Path '.vm-targets.json'
    if (Test-Path -LiteralPath $cwd -PathType Leaf) { return $cwd }
    Join-Path (Get-OsConfigDir) '.vm-targets.json'
}

$ConfigPath = Resolve-ConfigPath
$ConfigDir  = Split-Path -Parent $ConfigPath
if (-not $ConfigDir) { $ConfigDir = (Get-Location).Path }   # bare filename -> current dir
$CredDir    = Join-Path $ConfigDir '.vm-creds'

function Read-Config {
    if (-not (Test-Path -LiteralPath $ConfigPath)) {
        return [pscustomobject]@{ current = $null; targets = [pscustomobject]@{} }
    }
    Get-Content -Raw -LiteralPath $ConfigPath | ConvertFrom-Json
}

function Write-Config($cfg) {
    # Write to a unique temp file then atomically replace, so a concurrent reader
    # never sees a truncated/partial config (two `use` calls at once = last writer wins,
    # cleanly, instead of a torn file). PID keeps the temp name collision-free.
    if ($ConfigDir -and -not (Test-Path -LiteralPath $ConfigDir)) {
        New-Item -ItemType Directory -Path $ConfigDir -Force | Out-Null
    }
    $tmp = "$ConfigPath.$PID.tmp"
    $cfg | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $tmp -Encoding UTF8
    Move-Item -LiteralPath $tmp -Destination $ConfigPath -Force
}

function Get-TargetDef($cfg, $name) {
    if (-not $name) { throw "No target specified and no 'current' target set. Run: vm use <name>" }
    $def = $cfg.targets.$name
    if (-not $def) { throw "Unknown target '$name'. Known: $($cfg.targets.PSObject.Properties.Name -join ', ')" }
    $def
}

function Invoke-OnTarget($def, [string]$commandLine) {
    switch ($def.type) {
        'hyperv' {
            $params = @{ VMName = $def.vmName }
            if ($def.credPath) {
                if (-not (Test-Path $def.credPath)) {
                    throw "Credential file '$($def.credPath)' missing. Run: vm save-cred <name>"
                }
                $params.Credential = Import-Clixml $def.credPath
            }
            # Run the command line inside the guest so native commands, pipelines and
            # exit codes behave as if typed at a prompt.
            # Native guest exit codes don't cross PowerShell Direct on their own, so the
            # guest emits a trailing sentinel that we strip here while output still streams.
            Invoke-Command @params -ScriptBlock {
                param($cmd)
                $global:LASTEXITCODE = 0
                Invoke-Expression $cmd
                "__VMEXIT__:$LASTEXITCODE"
            } -ArgumentList $commandLine | ForEach-Object {
                if ($_ -is [string] -and $_ -match '^__VMEXIT__:(\d+)$') {
                    $script:TargetExit = [int]$Matches[1]
                } else { $_ }
            }
        }
        'ssh' {
            $sshArgs = @()
            if ($def.key)  { $sshArgs += @('-i', $def.key) }
            if ($def.port) { $sshArgs += @('-p', "$($def.port)") }
            $sshArgs += @('-o', 'BatchMode=yes')           # never hang on a password prompt
            if ($def.options) { foreach ($o in $def.options) { $sshArgs += @('-o', $o) } }
            $dest = if ($def.user) { "$($def.user)@$($def.host)" } else { $def.host }
            $sshArgs += @($dest, $commandLine)
            & ssh @sshArgs
            $script:TargetExit = $LASTEXITCODE
        }
        'wsl' {
            $wslArgs = @()
            if ($def.distro) { $wslArgs += @('-d', $def.distro) }
            if ($def.user)   { $wslArgs += @('-u', $def.user) }
            $wslArgs += @('--', 'bash', '-lc', $commandLine)
            & wsl @wslArgs
            $script:TargetExit = $LASTEXITCODE
        }
        default { throw "Unsupported target type '$($def.type)'." }
    }
}

# ---- subcommand dispatch ------------------------------------------------------
$cfg = Read-Config
$first = if ($Rest.Count -gt 0) { $Rest[0] } else { $null }

switch ($first) {
    'list' {
        if (-not $cfg.targets.PSObject.Properties.Name) { Write-Host "No targets configured."; break }
        $cfg.targets.PSObject.Properties | ForEach-Object {
            $marker = if ($_.Name -eq $cfg.current) { '*' } else { ' ' }
            "{0} {1,-12} {2,-7} {3}" -f $marker, $_.Name, $_.Value.type,
                ($_.Value.vmName ?? $_.Value.host ?? $_.Value.distro)
        }
        break
    }
    'use' {
        $name = $Rest[1]
        $null = Get-TargetDef $cfg $name      # validate
        $cfg.current = $name
        Write-Config $cfg
        Write-Host "Active target -> $name"
        break
    }
    'save-cred' {
        $name = $Rest[1]
        if (-not $name) { throw "Usage: vm save-cred <name>" }
        if (-not (Test-Path $CredDir)) { New-Item -ItemType Directory -Path $CredDir | Out-Null }
        $path = Join-Path $CredDir "$name.xml"
        (Get-Credential -Message "Guest credentials for '$name'") | Export-Clixml $path
        Write-Host "Saved (DPAPI, current user) -> $path"
        Write-Host "Set `"credPath`": `"$path`" on target '$name' in $ConfigPath"
        break
    }
    default {
        # Treat all of $Rest as a command line to run on the chosen target.
        if (-not $Rest -or $Rest.Count -eq 0) {
            Write-Host "Usage: vm [-Target <name>] <command...>   |   vm list|use|save-cred"
            break
        }
        $name = if ($Target) { $Target } else { $cfg.current }
        $def  = Get-TargetDef $cfg $name
        $commandLine = $Rest -join ' '
        $script:TargetExit = 0
        Invoke-OnTarget $def $commandLine    # command output streams straight through
        exit ($script:TargetExit ?? 0)
    }
}
