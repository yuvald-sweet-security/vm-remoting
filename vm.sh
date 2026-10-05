#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

if [[ ${1:-} == save-cred ]]; then
    exec "${VM_REMOTING_PWSH:-pwsh}" -NoProfile -File "$script_dir/vm.ps1" "$@"
fi

if [[ -n ${VM_REMOTING_MCP:-} ]]; then
    binary=$VM_REMOTING_MCP
elif command -v vm-remoting-mcp >/dev/null 2>&1; then
    binary=vm-remoting-mcp
elif [[ -x $script_dir/target/release/vm-remoting-mcp ]]; then
    binary=$script_dir/target/release/vm-remoting-mcp
elif [[ -x $script_dir/target/debug/vm-remoting-mcp ]]; then
    binary=$script_dir/target/debug/vm-remoting-mcp
else
    printf '%s\n' 'vm-remoting-mcp is missing. Run cargo build or cargo install --path .' >&2
    exit 1
fi

exec "$binary" --cli "$@"
