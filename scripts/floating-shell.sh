#!/usr/bin/env bash
# The floating pane's process: a normal interactive login shell.
#
# floax's defining behavior is a session that survives toggling. The toggle
# hides the pane by closing it, so to preserve state across open/close we run
# the shell inside a per-workspace detached session (dtach/abduco/tmux) and
# re-attach on every open. No multiplexer installed? Degrade to a plain login
# shell — still fully usable, just fresh each time it's reopened.
#
# NOTE: the starting directory arrives via $HERDR_FLOAX_CWD, not herdr's --cwd
# flag. In herdr 0.7.1, `plugin pane open --cwd <path>` makes the new plugin
# pane exit immediately (it vanishes), so we set the directory here instead.
set -euo pipefail

state_dir="${HERDR_PLUGIN_STATE_DIR:-${TMPDIR:-/tmp}}"
bin="${HERDR_PLUGIN_ROOT:-.}/target/release/herdr-floax"
session="$("$bin" --session-key)"
backend="$("$bin" --session-backend)"

cd "${HERDR_FLOAX_CWD:-$HOME}" 2>/dev/null || cd "$HOME" 2>/dev/null || true

# Keep using a live session's original backend if PATH has changed.
# dtach: attach-or-create (-A); -z disables the suspend key.
if [ "$backend" = dtach ] || { [ -z "$backend" ] && command -v dtach >/dev/null 2>&1; }; then
  exec dtach -A "$state_dir/$session.dtach" -z "$bin" --session-shell dtach
# abduco: -A attach-or-create a session named per workspace.
elif [ "$backend" = abduco ] || { [ -z "$backend" ] && command -v abduco >/dev/null 2>&1; }; then
  exec abduco -A "$session" "$bin" --session-shell abduco
# tmux: new-session-or-attach to a per-workspace session in its own server.
elif [ "$backend" = tmux ] || { [ -z "$backend" ] && command -v tmux >/dev/null 2>&1; }; then
  # An existing tmux server retains the environment from its first launch.
  # Pass this workspace's context explicitly to each new session.
  context=()
  while IFS= read -r name; do
    case "$name" in
      HERDR_*|SHELL) context+=("$name=${!name}") ;;
    esac
  done < <(compgen -e)
  exec tmux -L "herdr-floax" new-session -A -s "$session" \
    env "${context[@]}" "$bin" --session-shell tmux
fi

exec "$bin" --session-shell plain
