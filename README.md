# xdg-desktop-portal-layercapture

An `org.freedesktop.impl.portal.InputCapture` backend for the [niri](https://github.com/niri-wm/niri)
Wayland compositor. It makes KDE Connect's **Share input devices** work on niri: push the
pointer past a screen edge, and your laptop's mouse and keyboard control the phone until you
move back off the phone's edge.

## Why

KDE Connect asks xdg-desktop-portal for an InputCapture session. niri has no InputCapture
backend (niri issue #823). xdg-desktop-portal-gnome only provides one on top of mutter, so on
niri KDE Connect logs `Couldn't create input capture session` and the feature stays dead.

This backend builds InputCapture from Wayland protocols niri already supports and serves it
to xdg-desktop-portal in niri sessions only.

## Status and compatibility

Tested with:

| Component | Version |
|---|---|
| niri | niri-spicy 26.04 (a niri fork) |
| xdg-desktop-portal | 1.20.4 |
| KDE Connect (desktop) | 26.04.3 |
| libei | 1.5.0 |

Upstream niri offers the same protocols, but it has not been tested.

**Phone side.** The KDE Connect Android app needs:
- the **Input devices receiver** plugin enabled;
- the **Mouse receiver** plugin enabled and granted the accessibility permission.

Without them, the phone never hands control back, and you have to use one of the
[escape hatches](#escape-hatches).

**Clients.** Only KDE Connect may create sessions: the app id `org.kde.kdeconnect.daemon` (and
the other `org.kde.kdeconnect.*` ids), or an empty app id from a `kdeconnectd` executable.
Other InputCapture clients (e.g. input-leap, lan-mouse, Deskflow) are refused. Calls are
accepted only from the xdg-desktop-portal frontend.

## How it works

- A 1 px transparent **layer-shell strip** sits on the Overlay layer along the edge KDE Connect
  asked for. It stays 8 px clear of the output corners, so niri's hot corner keeps working.
- Pushing against the strip adds up the outward relative motion. Once it passes the threshold
  (24 px by default), the backend captures:
  - a **pointer lock**, with relative motion;
  - **exclusive keyboard focus**;
  - a **keyboard-shortcuts inhibitor**, so most niri binds go to the phone.
- Captured input goes to KDE Connect over an **EIS server** (the libei protocol, via
  [reis](https://crates.io/crates/reis)). niri's keymap is passed along.
- When KDE Connect releases the capture, the cursor is put back just inside the edge.

Wayland globals it requires: `wl_compositor`, `wl_shm`, `wl_seat`, `zwlr_layer_shell_v1`,
`zxdg_output_manager_v1`, `zwp_pointer_constraints_v1` and `zwp_relative_pointer_manager_v1`.
`zwp_keyboard_shortcuts_inhibit_manager_v1` and `wp_cursor_shape_manager_v1` are used when
present.

## Installation on NixOS

The package needs rustc ≥ 1.95. nixos-26.05 has it.

### With the flake module

```nix
# flake.nix
{
  inputs.layercapture = {
    url = "github:iNViTiON/xdg-desktop-portal-layercapture";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, layercapture, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        layercapture.nixosModules.default
        { services.xdg-desktop-portal-layercapture.enable = true; }
        # ...
      ];
    };
  };
}
```

The module does three things:
- adds the package to `xdg.portal.extraPortals`, which installs the `.portal` file, the D-Bus
  activation file and the systemd user unit;
- puts the binary on `PATH`;
- sets `xdg.portal.config.niri."org.freedesktop.impl.portal.InputCapture" = "layercapture"`.
  Other desktops are not affected.

`xdg.portal.config.niri` becomes `/etc/xdg/xdg-desktop-portal/niri-portals.conf`, and that
file *replaces* niri's own `niri-portals.conf`. `programs.niri.enable = true` fills in the rest
of it (`default = gnome;gtk`, …). If you run niri some other way, set the full
`xdg.portal.config.niri` yourself. Otherwise the module warns, because every other portal
would stop working in niri.

### With the overlay

With the same flake input as above, instead of the module:

```nix
modules = [
  (
    { pkgs, ... }:
    {
      nixpkgs.overlays = [ layercapture.overlays.default ];
      # extraPortals also puts the binary on PATH.
      xdg.portal.extraPortals = [ pkgs.xdg-desktop-portal-layercapture ];
      xdg.portal.config.niri."org.freedesktop.impl.portal.InputCapture" = "layercapture";
    }
  )
];
```

### After switching

1. **Restart xdg-desktop-portal** (or log out and back in). It picks its backends only at
   startup, so the one that was running before the switch keeps routing InputCapture
   elsewhere:
   `systemctl --user restart xdg-desktop-portal.service`
   If `SupportedCapabilities` still reads 0 afterwards, the session bus has not picked up the
   new D-Bus service file yet (`busctl --user list --activatable | grep layercapture` is
   empty): log out and back in.
2. Restart KDE Connect, e.g.
   `systemctl --user restart app-org.kde.kdeconnect.daemon@autostart.service`, or reconnect the
   phone.
3. [Check that it is active](#check-that-it-is-active).

## Installation on other distributions

Build as your user with cargo ≥ 1.95, then install as root:

```bash
make                         # cargo build --release --locked
sudo make install PREFIX=/usr
```

This installs:
- `/usr/bin/xdg-desktop-portal-layercapture`;
- `/usr/share/xdg-desktop-portal/portals/layercapture.portal`;
- `/usr/share/dbus-1/services/org.freedesktop.impl.portal.desktop.layercapture.service`;
- `/usr/lib/systemd/user/xdg-desktop-portal-layercapture.service`;
- the README, NOTICE and LICENSE under `/usr/share/doc` and `/usr/share/licenses`.

`make install` never builds. Run `make` first as your normal user.

Make variables:
- `PREFIX` (default `/usr/local`), `BINDIR`, `DATADIR`;
- `SYSTEMDUSERUNITDIR` (default `$(PREFIX)/lib/systemd/user`), `DBUSSERVICEDIR`, `PORTALDIR`;
- `DESTDIR`, for staged installs.

`make install-data` installs only the portal, D-Bus and systemd files. `make uninstall` removes
everything `make install` put in place.

### Route InputCapture to the backend

xdg-desktop-portal reads **only the first** `niri-portals.conf` it finds, in this order:
1. `~/.config/xdg-desktop-portal/niri-portals.conf`;
2. `/etc/xdg/xdg-desktop-portal/niri-portals.conf`;
3. the one niri ships, usually `/usr/share/xdg-desktop-portal/niri-portals.conf`.

So the file you write must be a **complete copy** of niri's, plus one line:

```bash
mkdir -p ~/.config/xdg-desktop-portal
cp /usr/share/xdg-desktop-portal/niri-portals.conf ~/.config/xdg-desktop-portal/
echo 'org.freedesktop.impl.portal.InputCapture=layercapture' >> ~/.config/xdg-desktop-portal/niri-portals.conf
```

The result looks something like this:

```ini
[preferred]
default=gnome;gtk;
org.freedesktop.impl.portal.Access=gtk;
org.freedesktop.impl.portal.Notification=gtk;
org.freedesktop.impl.portal.Secret=gnome-keyring;
org.freedesktop.impl.portal.InputCapture=layercapture
```

Use `/etc/xdg/xdg-desktop-portal/` instead for all users. If you already have one of these
files, add the line to it instead.

### Activate

```bash
systemctl --user daemon-reload
```

Then log out and back in. Alternatively, reload the bus and restart the portal:

```bash
busctl --user call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig
systemctl --user restart xdg-desktop-portal.service
```

Then restart kdeconnectd, or reconnect the phone.

## Check that it is active

```bash
busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
    org.freedesktop.portal.InputCapture SupportedCapabilities
```

`u 3` means InputCapture is routed to this backend. An error, or `u 0`, means it is not (see
[Troubleshooting](#troubleshooting)).

The session must identify as niri (`XDG_CURRENT_DESKTOP=niri`); the `.portal` file has
`UseIn=niri`.

## KDE Connect setup

1. On the computer, open KDE Connect's settings for the paired phone and enable the **Share
   input devices** plugin.
2. In that plugin's settings, choose the screen edge that leads to the phone. The default is
   **left**.
3. On the phone, enable the receiver plugins listed under
   [Status and compatibility](#status-and-compatibility).

## Usage

- Move the pointer to the chosen edge and **keep pushing**. It takes a firm push of about
  24 px of motion past the edge, so brushing the edge or flicking into the hot corner does not
  trigger it. The cursor disappears, and the mouse and keyboard now drive the phone.
- To come back, move off the phone's matching edge. KDE Connect hands control back, and the
  cursor reappears just inside the edge.
- After a capture that the backend ended itself (Mod+Escape, `release`, timeouts), the edge
  stays inactive for 3 s, so it doesn't grab again immediately.

## Escape hatches

If control does not come back, use one of these:

1. **Mod+Escape.** niri's "toggle keyboard shortcuts inhibit" bind ends the capture. This
   works only if niri honours the inhibitor on a layer surface. It did on niri-spicy.
2. **A niri bind that bypasses the inhibitor** (recommended). Add it to your niri config:
   ```kdl
   binds {
       Mod+Shift+Escape allow-inhibiting=false { spawn "xdg-desktop-portal-layercapture" "release"; }
   }
   ```
   `xdg-desktop-portal-layercapture release` signals the running instance through its PID file
   (`$XDG_RUNTIME_DIR/layercapture/pid`). You can also run it from any terminal you can reach.
3. **Automatic releases.** A capture ends by itself when:
   - keyboard or pointer focus is lost (lock screen, screenshot UI, …);
   - no input is captured for 120 s;
   - it has lasted 30 min;
   - KDE Connect stops reading input.
4. **Watchdog.** If the event loop stalls for 3 s while a grab is held, a watchdog thread cuts
   the Wayland connection, and niri drops every grab. If the loop stays stalled for 30 s, the
   process exits.
5. **From a TTY.** Switch with Ctrl+Alt+F3, log in, and run either command:
   ```bash
   pkill -KILL -x layercapture
   systemctl --user kill -s KILL xdg-desktop-portal-layercapture.service
   ```
   The process names itself `layercapture`. niri frees every grab when it dies. Afterwards,
   restart kdeconnectd (see [Limitations](#limitations)).

## Configuration

The service runs `xdg-desktop-portal-layercapture` with no arguments, which is the same as
`serve`. Options must come after `serve`.

| Flag | Default | Meaning |
|---|---|---|
| `--pressure <px>` | 24 | Outward push (px of relative motion) needed to start capturing. |
| `--max-slope <ratio>` | 0.5 | A motion only counts as a push if its along-edge part ≤ ratio × its outward part. |
| `--corner-margin <px>` | 8 | Keep the edge strips this far from output corners (niri's hot corner). |
| `--idle-release <s>` | 120 | End a capture after this long without captured input (0 = never). |
| `--max-activation <s>` | 1800 | End any capture after this long (0 = never). |
| `--max-grab <s>` | off | Development cap: end any capture after this long; the watchdog also cuts the Wayland connection shortly after. |

Testing only. Don't use these in normal operation:
- `--allow-app-id <id>` (repeatable): also accept this app id.
- `--trust-any-caller`: disables the caller and app id checks.
- `--fake-zones <WxH>`: no Wayland, one fake zone.
- `--dev-control`: exports a debug D-Bus interface.

Logging uses `RUST_LOG`. The default is `info,zbus=warn`.

**Other distributions:** override `ExecStart` with a drop-in (`systemctl --user edit
xdg-desktop-portal-layercapture.service`). The empty `ExecStart=` clears the original line:

```ini
[Service]
ExecStart=
ExecStart=/usr/bin/xdg-desktop-portal-layercapture serve --pressure 40 --idle-release 300
Environment=RUST_LOG=debug
```

**NixOS:**

```nix
{ config, lib, ... }:
{
  systemd.user.services.xdg-desktop-portal-layercapture.serviceConfig.ExecStart = [
    ""
    "${lib.getExe config.services.xdg-desktop-portal-layercapture.package} serve --pressure 40"
  ];
}
```

This is written as a drop-in for the packaged unit. With the overlay, use
`lib.getExe pkgs.xdg-desktop-portal-layercapture`.

The new flags take effect when the service restarts. That breaks KDE Connect's session, so
restart kdeconnectd afterwards.

## Limitations

- **Never forwarded:** Mod+wheel and other scroll binds, 3- and 4-finger touchpad gestures,
  Mod+middle-drag, and VT switching and power keys. niri handles these before any client sees
  them.
- **Multiple monitors:** a barrier must lie on an outer edge of the desktop. A barrier on an
  edge shared with a neighbouring output is rejected, so pick an edge with nothing beyond it.
  niri also limits keyboard focus to layers on the active output, which makes captures on a
  second monitor less reliable.
- **KDE Connect quirks:**
  - Touchpad and TrackPoint scrolling follow the laptop's direction on the phone (tested).
    Mouse-wheel scrolling takes a different path in KDE Connect (it negates discrete steps)
    and may scroll the other way; this is untested.
  - Keyboard shortcuts with Super/Mod are passed to KDE Connect (niri does not act on them
    during a capture), but the phone generally ignores them. The same happens on Plasma.
  - KDE Connect never reconnects a lost session. **Any backend restart, crash or EIS
    disconnect disables sharing** until you restart kdeconnectd
    (`systemctl --user restart app-org.kde.kdeconnect.daemon@autostart.service`), toggle the
    plugin, or reconnect the phone. The backend logs this at error level when it happens.
- **Portal API:** only InputCapture version 1 is implemented. The v2 API of
  xdg-desktop-portal ≥ 1.21 (`CreateSession2`/`Start`) is not.
- One capture at a time. With several sessions, the newest barrier wins.

## Troubleshooting

- **Backend log:**
  ```bash
  journalctl --user -u xdg-desktop-portal-layercapture
  ```
  Every CreateSession is logged with its app id (and PID and executable for an empty app id),
  as is every refusal and its reason.
- **`SupportedCapabilities` is not `u 3`:** routing is missing.
  - Check which `niri-portals.conf` wins (see above).
  - Check that `layercapture.portal` is in a `xdg-desktop-portal/portals` directory on
    `XDG_DATA_DIRS`. On NixOS it must be in `/run/current-system/sw/share/xdg-desktop-portal/portals`.
  - Check that you logged out and back in after installing.
  - `journalctl --user -u xdg-desktop-portal` shows why the frontend skipped the backend.
- **kdeconnectd logs `Couldn't create input capture session`:** same cause, InputCapture is
  not routed to this backend. Fix routing, then restart kdeconnectd. The kdeconnectd log is
  usually under `journalctl --user -u app-org.kde.kdeconnect.daemon@autostart.service`.
- **Control doesn't come back from the phone** (only Mod+Escape ends the capture):
  the phone's release message is late. kdeconnectd's log (`journalctl --user -u
  app-org.kde.kdeconnect.daemon@autostart.service`) shows `releasing with ...` only seconds
  later, typically while the phone is linked over Bluetooth instead of Wi-Fi (e.g. right after
  restarting kdeconnectd). Wait for the Wi-Fi link, or toggle KDE Connect on the phone. The
  backend logs late releases as `Release while no capture is active`.
- **Crossing did nothing:**
  - Push harder; it takes about 24 px of motion past the edge.
  - Check the edge chosen in the plugin settings.
  - Check that the phone has the receiver plugins.
  - If the backend restarted since kdeconnectd started, restart kdeconnectd.
- **Watchdog:** its actions are appended to `$XDG_RUNTIME_DIR/layercapture/watchdog.log`.
  systemd removes that directory whenever the service stops or restarts, so read it while the
  service is still running.

## Development

```bash
direnv allow        # or: nix develop
cargo build
cargo test          # unit tests; no Wayland or session bus needed
nix build .#default # the package, with tests
```

The dev shell pins the same nixpkgs as the package (rustc 1.95). It also provides
`ei-debug-events`, `xkbcli`, `dbus-run-session` and `wayland-info`.

Before `nix build` or `nix flake check`, `git add` new files: the flake sees only tracked
files.

### D-Bus contract test

`dev/contract-test.sh` runs the backend on a private session bus with `WAYLAND_DISPLAY`
unset (so nothing can be grabbed), and runs `examples/contract.rs` against it:

```bash
./dev/contract-test.sh
```

### Test tools

These run in your live niri session:

- **`probe --no-grab`** maps a strip on the left edge of an output and logs contacts, relative
  motion and push distances. It prints `WOULD ACTIVATE` where a capture would start. It never
  grabs. Use it to tune `--pressure` and `--max-slope`.
- **`probe`** grabs once on a push, prints everything captured (keys, buttons, scroll,
  keymap), releases after `--auto-release` seconds (default 5, max 8) and exits. A watchdog
  hard limit backs up the auto-release. Before running it, make sure you know the
  [escape hatches](#escape-hatches) and have the `release` bind. During development, give the
  bind the absolute path of `target/debug/xdg-desktop-portal-layercapture`.
  - `--eis debug-events|dump` forwards the capture over EIS.
  - `--stall` freezes the event loop to test the watchdog.
- **`eis-demo`** needs no grab. It plays scripted input with niri's real keymap over the same
  kind of socket that ConnectToEIS returns, to libei's `ei-debug-events` (default) or to the
  built-in dump receiver:
  ```bash
  xdg-desktop-portal-layercapture eis-demo
  xdg-desktop-portal-layercapture eis-demo --client dump --keymap-out /tmp/keymap.xkb
  xdg-desktop-portal-layercapture eis-demo --fault double-start   # libei must disconnect
  ```
- **`examples/ic_client.rs`** is an ashpd client that mimics KDE Connect through the real
  frontend. Run it in a named scope so it gets an app id, and allow that app id with
  `serve --allow-app-id … --max-grab 10`:
  ```bash
  systemd-run --user --scope --unit=app-layercapture.test-$$.scope target/debug/examples/ic_client
  ```

### Temporary routing without a rebuild (NixOS)

`dev/xdp-override.sh enable|disable|status` routes the live frontend's InputCapture to a
backend you run by hand. It:
- points `NIX_XDG_DESKTOP_PORTAL_DIR` at a directory with the system portals plus this one;
- writes a complete `~/.config/xdg-desktop-portal/niri-portals.conf`;
- restarts xdg-desktop-portal, so screen sharing blips.

Start `serve` in a terminal before `enable`. `disable` restores everything.

## License and credits

MIT. See [LICENSE](LICENSE).

Design references (see [NOTICE](NOTICE)):
- [Qingswe/niri-input-portal](https://github.com/Qingswe/niri-input-portal) (MIT): the
  layer-shell + pointer-lock InputCapture design for niri;
- emersion/xdg-desktop-portal-wlr PR #359 (MIT): EIS keymap and modifier forwarding;
- [ids1024/reis](https://github.com/ids1024/reis) (MIT): EIS server examples;
- [gfhdhytghd/hypr-kdeconnect-fix](https://github.com/gfhdhytghd/hypr-kdeconnect-fix) (MIT):
  portal caller checks and data files.

feschber/lan-mouse (GPL-3.0) and KWin were consulted for behaviour only; no code was copied.
xdg-desktop-portal, xdg-desktop-portal-gnome/-kde and KDE Connect were read as references for
the protocol contract.
