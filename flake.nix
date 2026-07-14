{
  description = "Carrot - A pure Rust tiling Wayland compositor with zero linked C, all the way down to the kernel.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    flake-compat = {
      url = "github:NixOS/flake-compat";
      flake = false;
    };
  };

  outputs =
    {
      crane,
      flake-parts,
      ...
    }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      flake.nixosModules.default =
        { config, lib, pkgs, ... }:
        let
          cfg = config.programs.carrot;
          package = inputs.self.packages.${pkgs.stdenv.hostPlatform.system}.carrot;
        in
        {
          options.programs.carrot.enable = lib.mkEnableOption "the carrot compositor";
          config = lib.mkIf cfg.enable {
            # xdg-utils rides along: xdg-open is what apps exec for links
            # and file managers, and nothing else guarantees it on PATH
            environment.systemPackages = [ package pkgs.xdg-utils ];
            # the package carries the session entry; this lists it at the DM
            services.displayManager.sessionPackages = [ package ];
            # carrot is its own screencast backend; the package ships the
            # portal registration and the preference file
            xdg.portal = {
              enable = true;
              extraPortals = [ package ];
              configPackages = [ package ];
            };
            # clients draw text through fontconfig; a bare system renders
            # tofu for emoji and symbols without the default set
            fonts.enableDefaultPackages = lib.mkDefault true;
          };
        };

      perSystem =
        {
          pkgs,
          lib,
          self',
          inputs',
          ...
        }:
        let
          craneLib = crane.mkLib pkgs;

          # nightly is mandatory: -Z build-std + eyra. rust-src for build-std.
          # pinned to taproot's rust-toolchain.toml so carrot and its libc
          # build on the same compiler.
          toolchain =
            (inputs'.fenix.packages.toolchainOf {
              channel = "nightly";
              date = "2026-06-11";
              sha256 = "sha256-L59udwZx36niu4S6j9huMpLBWL4m/Flt61nbXfXk/wk=";
            }).withComponents
              [
                "cargo"
                "rustc"
                "rust-src"
                "clippy"
                "rustfmt"
              ];

          # Only include source files that are actually relevant to the build
          src = lib.cleanSourceWith {
            src = ./.;
            filter = craneLib.filterCargoSources;
          };

          # Pure Rust, zero linked C - no dependencies to build against.

          commonArgs = {
            inherit src;
            pname = "carrot";
            version = "0.1.0";
            strictDeps = true;

            nativeBuildInputs = [ pkgs.makeWrapper ];

            # the keymap tests build real xkb state in the check phase
            XKB_CONFIG_ROOT = "${pkgs.xkeyboard-config}/share/X11/xkb";
          };

          carrot = craneLib.buildPackage (commonArgs // {
            cargoArtifacts = craneLib.buildDepsOnly commonArgs;

            postInstall = ''
              wrapProgram $out/bin/carrot \
                --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [ pkgs.vulkan-loader ]} \
                --set-default XKB_CONFIG_ROOT ${pkgs.xkeyboard-config}/share/X11/xkb

              # Wayland session desktop entry; DesktopNames makes the session
              # manager set XDG_CURRENT_DESKTOP=carrot, which the portal
              # frontend matches against carrot-portals.conf
              mkdir -p $out/share/wayland-sessions
              cat > $out/share/wayland-sessions/carrot.desktop << EOF
              [Desktop Entry]
              Name=Carrot
              Comment=A pure Rust tiling Wayland compositor
              Exec=$out/bin/carrot
              Type=Application
              DesktopNames=carrot
              EOF

              # the portal backend is the compositor itself - register the
              # bus name it serves and prefer it for screencasts
              mkdir -p $out/share/xdg-desktop-portal/portals
              cat > $out/share/xdg-desktop-portal/portals/carrot.portal << EOF
              [portal]
              DBusName=org.freedesktop.impl.portal.desktop.carrot
              Interfaces=org.freedesktop.impl.portal.ScreenCast
              UseIn=carrot
              EOF
              cat > $out/share/xdg-desktop-portal/carrot-portals.conf << EOF
              [preferred]
              default=*
              org.freedesktop.impl.portal.ScreenCast=carrot
              EOF
            '';

            passthru.providedSessions = [ "carrot" ];

            meta = {
              description = "A pure Rust tiling Wayland compositor with zero linked C, all the way down to the kernel";
              license = lib.licenses.gpl3;
              platforms = [ "x86_64-linux" "aarch64-linux" ];
              mainProgram = "carrot";
            };
          });
        in
        {
          packages = {
            default = self'.packages.carrot;
            carrot = carrot;
          };

          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              toolchain
              rust-analyzer
              binutils # readelf / nm for the zero-C gate

              # Vulkan debugging
              vulkan-tools          # vulkaninfo
              vulkan-validation-layers
              renderdoc

              # Wayland debugging
              wev                   # input event viewer
              wayland-utils         # wayland-info
            ];

            env = {
              LD_LIBRARY_PATH = lib.makeLibraryPath [ pkgs.vulkan-loader ];
              VK_LAYER_PATH = "${pkgs.vulkan-validation-layers}/share/vulkan/explicit_layer.d";
              # kbvm needs the xkb data root; nothing ships it system-wide on NixOS
              XKB_CONFIG_ROOT = "${pkgs.xkeyboard-config}/share/X11/xkb";
            };

            shellHook = ''
              echo "carrot development shell"
              echo "  cargo build              # build"
              echo "  cargo clippy             # lint"
              echo "  cargo run                # run"
              echo "  cargo clean              # clean"
            '';
          };

          formatter = pkgs.nixfmt-tree;
        };
    };
}
