#!/usr/bin/env bash
# Install missing bindings by command, preserving hand-written TOML and keys.
# Usage: install-keybinding.sh [TOGGLE_KEY] [INSTANCES_KEY]
# Defaults: HERDR_FLOAX_KEY or prefix+f; HERDR_FLOAX_INSTANCES_KEY or prefix+i.
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
exec "$root/target/release/herdr-floax" --install-keybindings "$@"
