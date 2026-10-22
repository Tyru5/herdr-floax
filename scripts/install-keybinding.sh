#!/usr/bin/env bash
# Add herdr-floax keybinds to herdr's config.toml (idempotent).
#
#   Usage: install-keybinding.sh [TOGGLE_KEY] [INSTANCES_KEY]
#          Defaults: $HERDR_FLOAX_KEY or prefix+f,
#                    $HERDR_FLOAX_INSTANCES_KEY or prefix+i.
#
# Override the key by passing it (`install-keybinding.sh prefix+g`), setting
# $HERDR_FLOAX_KEY, or editing the `key` line in config.toml afterward. Re-runs
# are safe: a guard marker prevents duplicate blocks.
set -euo pipefail

key="${1:-${HERDR_FLOAX_KEY:-prefix+f}}"
instances_key="${2:-${HERDR_FLOAX_INSTANCES_KEY:-prefix+i}}"
cfg="${HERDR_CONFIG_DIR:-$HOME/.config/herdr}/config.toml"
marker="# herdr-floax:keybind"
instances_marker="# herdr-floax:instances-keybind"

# Re-runs preserve installed blocks. Check their actual keys, not the defaults
# passed on this invocation, before adding the other binding.
configured_key() {
  awk -v marker="$1" '
    index($0, marker) { found=1; next }
    found && /^[[:space:]]*key[[:space:]]*=/ {
      sub(/^[^=]*=[[:space:]]*/, "")
      quote = substr($0, 1, 1)
      if (quote != "\"" && quote != sprintf("%c", 39)) exit 1
      value = substr($0, 2)
      # Do not guess at TOML escape decoding when comparing preserved keys.
      if (quote == "\"" && index(value, "\\")) exit 1
      end = index(value, quote)
      if (!end) exit 1
      print substr(value, 1, end - 1); exit
    }
  ' "$cfg"
}
if [ -f "$cfg" ]; then
  if grep -qF "$marker" "$cfg"; then
    key="$(configured_key "$marker")" || {
      echo "Cannot safely read the installed toggle key. Use unescaped key characters in $cfg." >&2
      exit 1
    }
  fi
  if grep -qF "$instances_marker" "$cfg"; then
    instances_key="$(configured_key "$instances_marker")" || {
      echo "Cannot safely read the installed picker key. Use unescaped key characters in $cfg." >&2
      exit 1
    }
  fi
fi

if [ "$key" = "$instances_key" ]; then
  echo "Toggle and instance picker keys must be different. Choose a different picker key or edit the existing binding." >&2
  exit 1
fi

if [ ! -e "$cfg" ]; then
  mkdir -p "$(dirname "$cfg")"
  : > "$cfg"
fi

if grep -qF "$marker" "$cfg" 2>/dev/null; then
  echo "herdr-floax keybind already present in $cfg (edit the '$marker' block to change it)."
else
  cat >> "$cfg" <<EOF

$marker
[[keys.command]]
key = "$key"
type = "plugin_action"
command = "herdr-floax.toggle"
description = "Toggle floating pane"
EOF
  echo "Added '$key' -> herdr-floax.toggle in $cfg."
fi

if grep -qF "$instances_marker" "$cfg" 2>/dev/null; then
  echo "herdr-floax instance picker keybind already present in $cfg."
else
  cat >> "$cfg" <<EOF

$instances_marker
[[keys.command]]
key = "$instances_key"
type = "plugin_action"
command = "herdr-floax.instances"
description = "List and navigate Floax sessions"
EOF
  echo "Added '$instances_key' -> herdr-floax.instances in $cfg."
fi
echo "Reload herdr config (or restart herdr) for it to take effect."
