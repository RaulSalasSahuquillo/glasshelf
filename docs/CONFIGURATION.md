# Configuration

GlassDock is configured with environment variables and one JSON file. There is deliberately no
other config file yet (see the roadmap in the README).

## Environment variables

Set them for the dock process. In Hyprland the simplest way is `env = NAME,value` in
`hyprland.conf`, placed **before** the `exec-once` line that starts the dock, or exported in the
shell that runs `glassdock-launch`.

| Variable | Default | Meaning |
| --- | --- | --- |
| `GLASSDOCK_OUTPUTS` | all outputs | Comma-separated output names (`hyprctl monitors`) on which to show the dock. Outputs that are not listed get no dock. |
| `GLASSDOCK_PINNED` | `$XDG_CONFIG_HOME/glassdock/pinned.json` (`~/.config/glassdock/pinned.json`) | Path of the pinned-apps file. |
| `GLASSDOCK_LANG` | derived from `LC_ALL`, `LC_MESSAGES`, `LANG` | `en` or `es`. Only the dock's own menu text is translated; app names come from `.desktop` files. |
| `GLASSDOCK_DEBUG` | unset | Any value turns on verbose logging of outputs, input and capture events on stderr. |
| `GLASSDOCK_BIN` | next to `glassdock-launch`, then `$PATH` | Which binary `glassdock-launch` runs. |
| `__EGL_VENDOR_LIBRARY_FILENAMES` | Mesa's vendor file if present | Standard glvnd variable. Set it yourself to choose another GPU; GlassDock will not override it. |

Example:

```ini
env = GLASSDOCK_OUTPUTS,eDP-1,DP-1
env = GLASSDOCK_LANG,en
exec-once = glassdock-launch
```

## Pinned apps

`~/.config/glassdock/pinned.json` is a JSON array of desktop-entry ids, in dock order:

```json
[
  "kitty.desktop",
  "firefox.desktop",
  "org.gnome.Nautilus.desktop",
  "code.desktop"
]
```

- Ids are the file names under `/usr/share/applications`, `~/.local/share/applications` and the
  Flatpak and Snap export directories.
- Entries that are not installed are skipped, not deleted.
- If the file does not exist, GlassDock **seeds** it in memory with the first *installed* app of
  each common category (terminal, browser, file manager, editor, ...). It is only written when you
  change something from the dock.
- A corrupt file is not touched at start-up: the dock runs with the seed. If you then change the
  pinned apps from the dock, the old file is first copied to `pinned.json.bak`.
- You normally never edit the file by hand: use the right-click menu (pin, unpin, move left or
  right) and the `+` button.

## Monitor layout

With `monitor = ,preferred,auto,1` Hyprland places external monitors in the order it detects them,
which can change from one session to the next, so the pointer may suddenly cross to a different
screen. Pin the positions:

```ini
monitor = eDP-1,    preferred, 0x0,    1.6
monitor = HDMI-A-1, preferred, 1600x0, 1
monitor = DP-1,     preferred, 3520x0, 1
```

The x offsets are in *logical* pixels: the width of a 2560 px panel at scale 1.6 is 1600. Monitor
names come from `hyprctl monitors`.

## Minimize

Hyprland has no minimize. The dock's menu moves the window to the special workspace `minimized`.
To do the same from the keyboard:

```ini
bind = SUPER,       H, movetoworkspacesilent, special:minimized
bind = SUPER SHIFT, H, togglespecialworkspace, minimized
```

## Layer rules

Do **not** apply a `layerrule = blur` to the `glassdock` namespace. Compositor blur is applied under
the whole layer surface, including its transparent shadow margin, and shows up as a dark halo around
the dock. The dock does its own refraction. If you copied a blur rule for another dock, scope it to
that dock's namespace.

## Appearance constants

Sizes and timings are constants in `src/dock.rs` (`ICON`, `GAP`, `ZOOM`, `ZOOM_RANGE`,
`HIDE_DELAY`, `K_SCALE`, `K_REVEAL`, `K_LABEL`) and the material is in `shaders/glass.frag`
(`BEVEL`, `FOLD`, `DISP`, ...). They are compiled in. To experiment with the shader without
rebuilding:

```bash
glassdockctl shader /path/to/glassdock/shaders
```

The dock recompiles `glass.vert` and `glass.frag` from that directory and keeps the previous
shader if the new one does not compile.
