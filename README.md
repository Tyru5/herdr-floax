# herdr-floax

A floating scratch shell for the current [herdr](https://herdr.dev) workspace —
inspired by [`tmux-floax`](https://github.com/omerxx/tmux-floax).

One keybinding toggles a floating pane: **open → reveal → dismiss**, one
instance per workspace. The floating pane is a real, fully-interactive herdr
pane (your login shell) — you can do anything in it that you can in a normal
pane. Its session persists across toggles.

**`prefix+i` opens the instance picker.** It shows the total number of running
shell sessions in the connected Herdr server, including dismissed sessions,
with each workspace's name and ID, open/detached state, backend, and launch
directory. Use **↑/↓** or **j/k** to select, **Enter** to jump or reattach,
**r** to refresh, and **Esc** or **q** to close. The directory is where the
session started, not necessarily the shell's current directory.

## How it works

Pressing the key toggles one floating pane per workspace:

- **no floating pane yet** → opens a centered, sized popup (default 98%×96% —
  generous, since the backdrop is dead space; see
  [Limitations](#limitations)) hosting your login shell, over a dimmed backdrop
- **focused on it** → dismisses it (closes the pane; the shell session survives)
- **exists but you focused away** → reveals + re-maximizes it

Under the hood the pane runs a small Rust TUI (`ratatui` + `portable-pty` +
`vt100`) that draws the floating box and embeds a real shell PTY inside it —
the same way herdr-file-viewer draws its help overlay, except the box hosts a
live terminal instead of static text. Everything you can do in a normal pane
works in the box: vim, REPLs, paste, colors, resize. Input is raw byte
passthrough, so no key handling is lost in translation.

The host pane itself is a herdr **split** that the toggle action immediately
zooms, so the box appears centered over the whole workspace. herdr's
`overlay`/`zoomed` placements can't be used from a keybinding — herdr tears
those transient views down the instant the invoking action finishes — and the
app-drawn box is what restores floax's sized-popup look on top of that
constraint.

- **Scope:** per workspace. Toggling in workspace A and workspace B gives you two
  independent floating shells.
- **Persistence:** dismiss *closes* the pane, so the embedded shell runs inside a
  per-workspace detached session when a multiplexer (`dtach`, `abduco`, or
  `tmux`) is on your `PATH` — anything running in it survives dismiss/reopen.
  Without one, it degrades to a plain login shell (fresh each reopen).
- **Backdrop:** see [Limitations](#limitations) — the area around the box uses
  your terminal background, not your live panes dimmed behind it.

The picker uses the same split-then-zoom approach, so it works without the
newer Herdr popup API. It never creates a shell just by opening the list.
Selecting an open session focuses and maximizes its pane. Selecting a detached
session opens a view in its original workspace and reattaches the running
shell. Closing the picker removes only its temporary split.
Herdr's public focus API navigates all attached UI clients in the same server;
it cannot target only the client that invoked the plugin.

Session supervisors hold OS file locks while their shells run. Unlocked
records do not count, even after a crash. Pane ownership uses pane and terminal
IDs rather than titles, so renaming a Floax pane does not break discovery.
Sessions are namespaced by Herdr socket identity and workspace ID. Other
Herdr servers and sessions left behind by a restarted server are not included.
A still-running shell whose workspace was closed is listed as `workspace
closed`, but cannot be navigated to. Without a multiplexer, plain shells count
only while running and cannot survive dismissal.

**Upgrading from 0.2.0's original session naming:** exit existing Floax shells
before upgrading. Old detached sessions are not adopted automatically because
their workspace-only names cannot safely identify the owning Herdr server.
Reopening after the upgrade starts tracked, server-scoped sessions. Re-run the
keybinding installer to add the picker binding; it preserves the existing
toggle binding. If the preserved toggle already uses `prefix+i`, choose another
picker key with the installer's second argument.

## Limitations

**The backdrop is not your real workspace.** In tmux-floax, the popup floats
over your *live* session — you see your actual panes, dimmed, behind the box.
herdr-floax cannot replicate that: the plugin only controls its own pane's
canvas. herdr owns the other panes' PTYs and provides no primitive for a plugin
to composite a persistent popup over them (its only true-overlay placement is
transient — it is torn down the moment the invoking keybinding action
completes). So the area around the floating box is a solid fill matching your
terminal's background, not your dimmed workspace showing through.

Related version-specific quirks this plugin works around (herdr 0.7.1):

- `overlay`/`zoomed` pane placements vanish when opened from a keybinding
  action — hence the split-then-zoom approach.
- `plugin pane open --cwd <path>` makes the new pane exit immediately — hence
  the starting directory travels via the `HERDR_FLOAX_CWD` env var and the
  shell script `cd`s itself.

If a future herdr adds a persistent sized-overlay primitive, the app-drawn box
(and this whole limitation) could be replaced by it.

## Configuration

Copy `floax.conf.example` to the plugin config dir
(`herdr plugin config-dir herdr-floax`, i.e.
`~/.config/herdr/plugins/config/herdr-floax/floax.conf`):

```conf
width_pct = 98    # box width, % of the pane (20..100)
height_pct = 96   # box height, % of the pane (20..100)
key_hint = prefix+f   # shown in the bottom border (display only)
# backdrop = #102030  # optional override; defaults to the terminal background
```

Env overrides per invocation: `HERDR_FLOAX_WIDTH_PCT`, `HERDR_FLOAX_HEIGHT_PCT`,
`HERDR_FLOAX_KEY_HINT`, `HERDR_FLOAX_BACKDROP`.

## Install

From GitHub (builds with `cargo` at install time):

```sh
herdr plugin install Tyru5/herdr-floax

# Then install the default keybind (prefix+f) and reload herdr config:
bash "$(herdr plugin list --plugin herdr-floax --json | jq -r '.result.plugins[0].plugin_root')/scripts/install-keybinding.sh"
```

Or for local development:

```sh
# 1. Link the plugin (no GitHub round-trip):
herdr plugin link /path/to/herdr-floax

# 2. Install the default keybind (prefix+f), then reload herdr config:
bash /path/to/herdr-floax/scripts/install-keybinding.sh
```

Requires Rust 1.89 or newer (`cargo`) to build. The GitHub installation command
above uses [`jq`](https://jqlang.github.io/jq/); runtime actions do not need it.
For session persistence, install `dtach`, `abduco`, or tmux 3.2 or newer.

## Keybinding

The defaults are **`prefix+f`** for toggle and **`prefix+i`** for the picker.
Change them any of these ways:

```sh
scripts/install-keybinding.sh prefix+g          # pass a key
HERDR_FLOAX_KEY=prefix+g scripts/install-keybinding.sh
scripts/install-keybinding.sh prefix+g prefix+G # toggle, then picker
HERDR_FLOAX_INSTANCES_KEY=prefix+G scripts/install-keybinding.sh
```

…or add/edit the block by hand in `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+f"
type = "plugin_action"
command = "herdr-floax.toggle"
description = "Toggle floating pane"

[[keys.command]]
key = "prefix+i"
type = "plugin_action"
command = "herdr-floax.instances"
description = "List and navigate Floax sessions"
```

The installer preserves existing bindings. If you installed the earlier picker
default `prefix+F`, edit its `herdr-floax.instances` block to use `prefix+i`.

Reload herdr's config (or restart herdr) after changing it.

## Files

| File | Purpose |
|---|---|
| `herdr-plugin.toml` | Manifest: floating shell and picker panes, toggle and instances actions, build. |
| `src/` | Rust TUI, embedded shell PTY, session inventory, toggle, and picker navigation. |
| `scripts/floating-shell.sh` | The embedded program: a login shell (detach-wrapped when possible). |
| `scripts/install-keybinding.sh` | Idempotently installs both overridable keybinds, including upgrades. |
| `floax.conf.example` | Size/hint configuration template. |
