# Troubleshooting

Start with the basics:

```bash
glassdockctl dump | python3 -m json.tool    # is it running? which outputs, scale, slots?
GLASSDOCK_DEBUG=1 glassdock                  # run in the foreground with logging (quit the launcher first)
journalctl -t glassdock -n 50                # messages from glassdock-launch (crashes, restarts)
```

## The dock does not appear

- `glassdock-launch: WAYLAND_DISPLAY is not set`: start it from inside the Hyprland session
  (`exec-once`, or a terminal in Hyprland), not from a TTY or over SSH.
- `cannot find the glassdock binary`: run `./scripts/install.sh`, or set `GLASSDOCK_BIN`.
- Nothing happens and `glassdockctl dump` says there is no socket: check `pgrep -a glassdock`. If the
  launcher is running but the dock keeps dying, `journalctl -t glassdock` shows the exit codes and
  the dock's stderr shows the reason.
- `no views` or an empty `dump`: GlassDock only creates a surface for outputs that exist. Check
  `hyprctl monitors`, and that `GLASSDOCK_OUTPUTS` (if set) contains real output names.
- It is there but hidden: that is the default. Push the pointer against the bottom edge, in the middle of the screen where the dock sits, or run
  `glassdockctl reveal all`.
- **Fullscreen windows hide it.** The dock sits on the `top` layer, below fullscreen windows. That is
  intended.

## "EGL" or "GLES" errors on start

GlassDock needs OpenGL ES 3.0 through EGL.

- Install your distribution's EGL and wayland-egl packages (`libegl1`, `libwayland-egl1` on
  Debian and Ubuntu, `mesa` and `wayland` on Arch).
- On a machine with only a proprietary GPU driver, unset the Mesa override:
  `env = __EGL_VENDOR_LIBRARY_FILENAMES,/usr/share/glvnd/egl_vendor.d/10_nvidia.json`.

## The glass looks flat, translucent, or has no refraction

The shader failed to compile, or the backdrop capture failed, and the dock is using its fallback.
Run it with `GLASSDOCK_DEBUG=1` and look for `shader` or `capture` lines. `wlr-screencopy` must be
available: `wayland-info | grep screencopy` (from `wayland-utils`) should list
`zwlr_screencopy_manager_v1`.

## A dark halo around the dock

You have a `layerrule = blur` that matches the dock. Remove it, or make sure it does not match the
`glassdock` namespace. See [CONFIGURATION.md](CONFIGURATION.md#layer-rules).

## Gray background behind the dock on white pages

Same cause as above, or a stale build: the current shader uses a luminance-adaptive shadow that stays
neutral over white. Rebuild and reinstall.

## The cursor turns white (or pixelated) over the dock

Seen on hybrid Intel and NVIDIA laptops when Hyprland uses software cursors. Use hardware cursors:

```ini
cursor {
    no_hardware_cursors = 0
}
```

(`0` means hardware cursors are enabled. If the cursor then disappears on your NVIDIA setup, try `1`
and report it.) The dock asks for the default arrow cursor with `wp-cursor-shape`; compositors that do not
implement that protocol keep whatever cursor the previous surface set.

## One of the monitors has a frozen or missing dock

- Hot-plugged monitors get a dock as soon as the compositor announces them. If one does not,
  `glassdockctl refresh`, then `glassdockctl dump` to see which outputs the dock knows.
- Monitor order changing between sessions is a Hyprland layout matter; pin the positions as shown in
  [CONFIGURATION.md](CONFIGURATION.md#monitor-layout).

## The `+` button does nothing

It needs `rofi`. Install it (`sudo apt install rofi`, `pacman -S rofi`, ...).

## Apps do not launch from the dock

GlassDock launches `.desktop` entries with `gio launch`. Install GLib's command-line tools
(`libglib2.0-bin` on Debian and Ubuntu, `glib2` on Arch) and check that the entry runs from a
terminal: `gio launch /usr/share/applications/kitty.desktop`.

## An XWayland app looks blurry next to the dock

That is XWayland scaling and not the dock. Use the application's native Wayland mode (for example
Electron apps: `--ozone-platform=wayland`) or set `xwayland { force_zero_scaling = true }` in
Hyprland and scale the app yourself.

## The discrete NVIDIA GPU wakes up

GlassDock sets the EGL vendor to Mesa unless `__EGL_VENDOR_LIBRARY_FILENAMES` is already defined.
Check that nothing in your session defines it for the dock, and that Mesa's file exists at
`/usr/share/glvnd/egl_vendor.d/50_mesa.json`. To see what holds the GPU:

```bash
cat /sys/bus/pci/devices/*/power_state 2>/dev/null
nvidia-smi --query-compute-apps=pid,name --format=csv
```

## Reporting a bug

Please open an issue with the bug-report template and include:

- `hyprctl version`, your GPU and driver, your monitors (`hyprctl monitors`),
- the output of `glassdockctl dump`,
- logs from `GLASSDOCK_DEBUG=1`,
- a screenshot if it is a visual problem.
