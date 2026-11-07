#!/usr/bin/env bash
# GlassDock uninstaller. Removes the installed binaries and examples.
#
#   ./scripts/uninstall.sh                  remove from ~/.local
#   ./scripts/uninstall.sh --prefix /usr/local
#   ./scripts/uninstall.sh --purge          also delete ~/.config/glassdock (your pinned apps)
#
# It does not edit hyprland.conf: remove the `exec-once = ...glassdock-launch` line yourself.

set -euo pipefail

prefix="${HOME}/.local"
purge=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix) [ "$#" -ge 2 ] || { echo "--prefix needs a directory" >&2; exit 1; }; prefix="$2"; shift 2 ;;
        --purge)  purge=1; shift ;;
        -h|--help) sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 1 ;;
    esac
done

if command -v glassdockctl >/dev/null 2>&1; then
    glassdockctl quit >/dev/null 2>&1 || true
fi

rm -f "$prefix/bin/glassdock" "$prefix/bin/glassdock-launch" "$prefix/bin/glassdockctl"
rm -rf "$prefix/share/glassdock"
echo "Removed GlassDock from $prefix"

if [ "$purge" -eq 1 ]; then
    cfg="${XDG_CONFIG_HOME:-$HOME/.config}/glassdock"
    rm -rf "$cfg"
    echo "Deleted $cfg"
fi

echo "Remember to remove the 'exec-once = ...glassdock-launch' line from your hyprland.conf."
