{
  description = "Carrot - A pure Rust tiling Wayland compositor with a Vulkan rendering pipeline.";

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
            filter = path: type:
              (craneLib.filterCargoSources path type)
              # Include protocol XML files if we vendor them
              || (lib.hasSuffix ".xml" path);
          };

          commonArgs = {
            inherit src;
            pname = "carrot";
            version = "0.1.0";

            nativeBuildInputs = with pkgs; [
              pkg-config
              wayland-scanner
            ];

            buildInputs = with pkgs; [
              # Vulkan
              vulkan-loader
              vulkan-headers

              # Wayland
              wayland
              wayland-protocols

              # DRM/KMS
              libdrm

              # Input
              libinput
              seatd

              # Keymap
              libxkbcommon

              # Misc
              pixman
              udev
            ];

            # Vulkan needs to be able to find the ICD at runtime
            LD_LIBRARY_PATH = lib.makeLibraryPath [ pkgs.vulkan-loader ];
          };

          carrot = craneLib.buildPackage (commonArgs // {
            cargoArtifacts = craneLib.buildDepsOnly commonArgs;

            # TODO: add session entry, portal config, etc
            postInstall = ''
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
              description = "A pure Rust tiling Wayland compositor with Vulkan rendering";
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
