# xdg-desktop-portal InputCapture backend for niri, so KDE Connect's "Share input devices"
# works. Written in nixpkgs style so it can move to
# pkgs/by-name/xd/xdg-desktop-portal-layercapture/package.nix later.
#
# Used by this repository's flake (packages, overlays.default, nixosModules.default). To enable
# it on NixOS, add the package to `xdg.portal.extraPortals` and route
# `xdg.portal.config.niri."org.freedesktop.impl.portal.InputCapture" = "layercapture"`; the
# flake's NixOS module does both.
{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "xdg-desktop-portal-layercapture";
  version = "0.1.0"; # keep in sync with Cargo.toml

  # In-repo build: only the files the build and tests need, so edits elsewhere in the tree
  # (README, dev scripts, target/) don't cause a rebuild. For nixpkgs, replace this with:
  #
  #   src = fetchFromGitHub {
  #     owner = "iNViTiON";
  #     repo = "xdg-desktop-portal-layercapture";
  #     tag = "v${finalAttrs.version}";
  #     hash = lib.fakeHash; # build once, paste the hash from the error
  #   };
  #   cargoHash = lib.fakeHash; # likewise, instead of cargoLock
  #
  # (and add `fetchFromGitHub` to the arguments).
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../Makefile
      ../src
      # `cargo test` also builds the examples.
      ../examples
      ../data
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # Pure Rust (wayland-client, zbus, reis): no native build inputs or libraries.

  # The unit tests use socketpairs, memfds and threads only; none needs a session bus or a
  # Wayland compositor.
  doCheck = true;

  # cargoInstallHook installs the binary. The Makefile installs the .portal file, the D-Bus
  # activation file and the systemd user unit, with @bindir@ replaced by $out/bin. NixOS only
  # picks up user units from lib/systemd/user (systemd.packages, which xdg.portal.extraPortals
  # feeds), which is the Makefile's default.
  postInstall = ''
    make install-data PREFIX=$out
  '';

  meta = {
    description = "xdg-desktop-portal InputCapture backend for niri (layer-shell barrier + EIS), for KDE Connect input sharing";
    homepage = "https://github.com/iNViTiON/xdg-desktop-portal-layercapture";
    license = lib.licenses.mit;
    # TODO: add a maintainer before submitting to nixpkgs.
    maintainers = [ ];
    platforms = lib.platforms.linux;
    mainProgram = "xdg-desktop-portal-layercapture";
  };
})
