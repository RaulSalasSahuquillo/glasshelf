# Security policy

GlassDock runs as an unprivileged user process inside your Wayland session. It reads your
`.desktop` files and icon themes, talks to Hyprland through its IPC sockets, captures a small
region of the screen behind the dock (kept in GPU memory only, never written to disk), and
exposes a local control socket at `$XDG_RUNTIME_DIR/glassdock.sock` (mode 0700 directory owned
by your user).

If you find a vulnerability, please do **not** open a public issue. Report it privately through
GitHub's "Report a vulnerability" button on the Security tab of the repository, including the
version (`glassdock --version`), your compositor version and steps to reproduce.
