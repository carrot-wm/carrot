{
  description = "Carrot - A pure Rust tiling Wayland compositor with zero linked C, all the way down to the kernel.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    crane.url = "github:ipetkov/crane";

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
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

      perSystem =
        {
          pkgs,
          lib,
          self',
          ...
        }:
        let
          craneLib = crane.mkLib pkgs;

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

            # TODO: portal config, etc
            postInstall = ''
              wrapProgram $out/bin/carrot \
                --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [ pkgs.vulkan-loader ]} \
                --set-default XKB_CONFIG_ROOT ${pkgs.xkeyboard-config}/share/X11/xkb

              # Wayland session desktop entry
              mkdir -p $out/share/wayland-sessions
              cat > $out/share/wayland-sessions/carrot.desktop << EOF
              [Desktop Entry]
              Name=Carrot
              Comment=A pure Rust tiling Wayland compositor
              Exec=$out/bin/carrot
              Type=Application
              EOF
            '';

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
            inputsFrom = [ carrot ];

            packages = with pkgs; [
              clippy
              rust-analyzer
              rustfmt

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
