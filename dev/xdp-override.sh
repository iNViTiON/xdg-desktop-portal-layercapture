#!/usr/bin/env bash
# Temporarily routes xdg-desktop-portal's InputCapture interface in the niri session to a
# locally built layercapture backend, without a NixOS rebuild. Every change is reverted by
# `disable`.
#
#   enable   save the current NIX_XDG_DESKTOP_PORTAL_DIR, point it at a directory with the
#            system .portal files plus ours, write a complete ~/.config niri-portals.conf with
#            the InputCapture line, and restart xdg-desktop-portal (portal users such as
#            screen sharing blip)
#   disable  restore the saved value, remove the conf and state, restart xdg-desktop-portal
#   status   show what is in effect
#
# The backend itself is not started here: run `serve` in a terminal before `enable` (the
# frontend D-Bus-activates the backend at startup, and without a service file only an
# already-running instance can answer).
set -euo pipefail

state_dir="$HOME/.cache/layercapture"
state="$state_dir/override.state"
portals="$state_dir/portals"
conf="$HOME/.config/xdg-desktop-portal/niri-portals.conf"
system_conf=/etc/xdg/xdg-desktop-portal/niri-portals.conf
system_portals=/run/current-system/sw/share/xdg-desktop-portal/portals
here="$(cd "$(dirname "$0")/.." && pwd)"

current_env() {
    systemctl --user show-environment | sed -n 's/^NIX_XDG_DESKTOP_PORTAL_DIR=//p'
}

case "${1:-status}" in
enable)
    if [ -e "$state" ]; then
        echo "already enabled (state file $state exists); run disable first" >&2
        exit 1
    fi
    if [ -e "$conf" ]; then
        echo "$conf already exists; refusing to overwrite it" >&2
        exit 1
    fi
    mkdir -p "$state_dir" "$portals" "$(dirname "$conf")"
    if systemctl --user show-environment | grep -q '^NIX_XDG_DESKTOP_PORTAL_DIR='; then
        printf 'set\t%s\n' "$(current_env)" >"$state"
    else
        printf 'unset\t\n' >"$state"
    fi
    rm -f "$portals"/*.portal
    ln -s "$system_portals"/*.portal "$portals"/
    ln -sf "$here/data/layercapture.portal" "$portals/layercapture.portal"
    { cat "$system_conf"; echo "org.freedesktop.impl.portal.InputCapture=layercapture"; } >"$conf"
    systemctl --user set-environment NIX_XDG_DESKTOP_PORTAL_DIR="$portals"
    systemctl --user restart xdg-desktop-portal.service
    sleep 1
    busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
        org.freedesktop.portal.InputCapture SupportedCapabilities
    echo "enabled; expect 'u 3' above"
    ;;
disable)
    if [ ! -e "$state" ]; then
        echo "not enabled (no $state)"
        exit 0
    fi
    IFS=$'\t' read -r mode value <"$state"
    if [ "$mode" = set ]; then
        systemctl --user set-environment NIX_XDG_DESKTOP_PORTAL_DIR="$value"
    else
        systemctl --user unset-environment NIX_XDG_DESKTOP_PORTAL_DIR
    fi
    rm -f "$conf" "$state"
    rm -rf "$portals"
    systemctl --user restart xdg-desktop-portal.service
    echo "disabled; NIX_XDG_DESKTOP_PORTAL_DIR=$(current_env)"
    ;;
status)
    if [ -e "$state" ]; then echo "override ENABLED ($state)"; else echo "override disabled"; fi
    echo "NIX_XDG_DESKTOP_PORTAL_DIR=$(current_env)"
    [ -e "$conf" ] && echo "user conf present: $conf" || echo "no user niri-portals.conf"
    busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
        org.freedesktop.portal.InputCapture SupportedCapabilities 2>/dev/null || true
    ;;
*)
    echo "usage: $0 enable|disable|status" >&2
    exit 2
    ;;
esac
