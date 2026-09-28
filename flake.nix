{
  description = "xdg-desktop-portal InputCapture backend for niri (layer-shell barrier + EIS)";

  # Same nixos-26.05 revision as ~/nixos-config, so the toolchain here is the one the NixOS
  # package will be built with (rustc 1.95) and store paths are shared.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/5e2305d577ca00acbba631b05cb1094d172b29f3";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in
    {
      packages.${system} = {
        default = self.packages.${system}.xdg-desktop-portal-layercapture;
        xdg-desktop-portal-layercapture = pkgs.callPackage ./nix/package.nix { };
      };

      overlays.default = final: prev: {
        xdg-desktop-portal-layercapture = final.callPackage ./nix/package.nix { };
      };

      # Installs the backend and routes InputCapture to it in niri sessions only.
      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.services.xdg-desktop-portal-layercapture;
        in
        {
          options.services.xdg-desktop-portal-layercapture = {
            enable = lib.mkEnableOption "the layercapture InputCapture portal backend for niri (KDE Connect \"Share input devices\")";
            package = lib.mkOption {
              type = lib.types.package;
              default = pkgs.callPackage ./nix/package.nix { };
              defaultText = lib.literalExpression "xdg-desktop-portal-layercapture from this flake";
              description = "The xdg-desktop-portal-layercapture package to use.";
            };
          };

          config = lib.mkIf cfg.enable {
            # Installs the .portal file, the D-Bus activation file and the systemd user unit
            # (xdg.portal feeds extraPortals to services.dbus.packages and systemd.packages).
            xdg.portal.extraPortals = [ cfg.package ];
            xdg.portal.config.niri."org.freedesktop.impl.portal.InputCapture" = "layercapture";

            # xdg.portal.config.niri becomes /etc/xdg/xdg-desktop-portal/niri-portals.conf, which
            # replaces niri's own niri-portals.conf entirely (first file wins). programs.niri
            # fills in the rest; without it, the file would hold only the line above and every
            # other portal would stop working in niri.
            warnings = lib.optional (!(config.xdg.portal.config.niri ? default)) ''
              services.xdg-desktop-portal-layercapture: xdg.portal.config.niri has no `default`
              entry, so /etc/xdg/xdg-desktop-portal/niri-portals.conf will route only InputCapture
              and shadow niri's own niri-portals.conf. Enable programs.niri, or set
              xdg.portal.config.niri to a complete copy of niri's niri-portals.conf.
            '';
          };
        };

      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          rustc
          cargo
          clippy
          rustfmt
          rust-analyzer
          # Test tools: ei-debug-events (libei), xkbcli (keymap comparison),
          # dbus-run-session / dbus-monitor, wayland-info.
          libei
          libxkbcommon
          dbus
          wayland-utils
          nixfmt
        ];
        RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
      };

      formatter.${system} = pkgs.nixfmt;
    };
}
