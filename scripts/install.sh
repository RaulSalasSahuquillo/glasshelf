#!/usr/bin/env bash
# GlassDock installer.
#
# Builds the dock from source and installs it for the current user. It never touches your
# Hyprland configuration unless you pass --autostart.
#
#   ./scripts/install.sh                 build + install into ~/.local
#   ./scripts/install.sh --autostart     ... and add `exec-once = glassdock-launch` to hyprland.conf
#   ./scripts/install.sh --prefix /usr/local     (needs write access to the prefix)
#   ./scripts/install.sh --no-build      reuse an existing target/release/glassdock
#
# Installed files:
#   <prefix>/bin/glassdock          the dock
#   <prefix>/bin/glassdock-launch   launcher that keeps it alive (use this in exec-once)
#   <prefix>/bin/glassdockctl       control tool
#   <prefix>/share/glassdock/       examples (Hyprland snippet, pinned.json)

set -euo pipefail

prefix="${HOME}/.local"
do_build=1
autostart=0
hypr_conf="${XDG_CONFIG_HOME:-$HOME/.config}/hypr/hyprland.conf"

usage() { sed -n '2,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }
info()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()   { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix)      [ "$#" -ge 2 ] || die "--prefix needs a directory"; prefix="$2"; shift 2 ;;
        --no-build)    do_build=0; shift ;;
        --autostart)   autostart=1; shift ;;
        --hypr-conf)   [ "$#" -ge 2 ] || die "--hypr-conf needs a file"; hypr_conf="$2"; shift 2 ;;
        -h|--help)     usage; exit 0 ;;
        *)             usage >&2; die "unknown option: $1" ;;
    esac
done

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

# sanity checks
if [ "$do_build" -eq 1 ]; then
    command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust: https://rustup.rs"
fi

ld_cache="$( (ldconfig -p 2>/dev/null || /sbin/ldconfig -p 2>/dev/null) || true)"
check_lib() {
    # Not `ldconfig -p | grep -q`: grep exits early, and with pipefail that reads as a failure.
    case "$ld_cache" in
        *"$1"*) ;;
        *) warn "runtime library $1 not found ($2)" ;;
    esac
}
check_lib libEGL.so.1        "install your distribution's libegl / mesa packages"
check_lib libwayland-egl.so.1 "install libwayland-egl1 (Debian/Ubuntu) or wayland (Arch)"
command -v rofi  >/dev/null 2>&1 || warn "rofi not found: the '+' button (add an app) will not work"
command -v gio   >/dev/null 2>&1 || warn "gio not found (libglib2.0-bin / glib2): apps cannot be launched from the dock"
command -v hyprctl >/dev/null 2>&1 || warn "hyprctl not found: GlassDock needs Hyprland to know about your windows"

# build 
if [ "$do_build" -eq 1 ]; then
    info "Building (release)..."
    cargo build --release --locked
fi
bin="${CARGO_TARGET_DIR:-$repo/target}/release/glassdock"
[ -x "$bin" ] || die "binary not found at $bin (build failed, or use a different CARGO_TARGET_DIR)"

# install
info "Installing into $prefix"
install -d "$prefix/bin" "$prefix/share/glassdock"
install -m 755 "$bin"                       "$prefix/bin/glassdock.new"
mv -f "$prefix/bin/glassdock.new"           "$prefix/bin/glassdock"
install -m 755 scripts/glassdock-launch     "$prefix/bin/glassdock-launch"
install -m 755 scripts/glassdockctl         "$prefix/bin/glassdockctl"
install -m 644 config/hyprland.conf.example "$prefix/share/glassdock/hyprland.conf.example"
install -m 644 config/pinned.example.json   "$prefix/share/glassdock/pinned.example.json"

case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) warn "$prefix/bin is not in your PATH; use absolute paths in hyprland.conf or add it to PATH" ;;
esac

#optional autostart
launch="$prefix/bin/glassdock-launch"
line="exec-once = $launch"
if [ "$autostart" -eq 1 ]; then
    [ -f "$hypr_conf" ] || die "Hyprland config not found: $hypr_conf (use --hypr-conf)"
    if grep -qE '^[[:space:]]*exec-once[[:space:]]*=.*glassdock-launch' "$hypr_conf"; then
        info "Autostart is already configured in $hypr_conf"
    else
        backup="$hypr_conf.glassdock-backup-$(date +%Y%m%d-%H%M%S)"
        cp -p "$hypr_conf" "$backup"
        printf '\n# GlassDock (added by scripts/install.sh)\n%s\n' "$line" >> "$hypr_conf"
        info "Added autostart to $hypr_conf (backup: $backup)"
    fi
fi

cat <<EOF

Done.

Start it now (from inside your Hyprland session):
    $launch &

Start it with Hyprland - add this line to your hyprland.conf (or re-run with --autostart):
    $line

More options (monitor layout, minimize keybinds, ...): $prefix/share/glassdock/hyprland.conf.example
EOF
