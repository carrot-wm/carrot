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

      flake = {
        homeModule = 
          { config, lib, pkgs, ... }:
          let
            inherit (lib)
              mkIf
              mkOption
              types
              ;
              cfg = config.carrot;
              actions = [];
              cfg_spring = lib.types.submodule {
                  options = {
                    damping_ratio = lib.mkOption {
                      type = lib.types.number;
                    };
                    stiffness = lib.mkOption {
                      type = lib.types.number;
                    };
                    epsilon = lib.mkOption {
                      type = lib.types.number;
                    };
                  };
                };
                cfg_ease = lib.types.submodule {
                    options = {
                      duration_ms = lib.mkOption {
                        type = lib.types.int;
                      };
                      curve = lib.mkOption {
                        type = lib.types.str;
                      };
                    };
                  };

              cfg_anim_kind = lib.types.submodule ({ config, ... }:{
                options = {
                  off = lib.mkOption {
                    type = lib.types.bool;
                  };
                  spring = lib.mkOption {
                    type = cfg_spring;
                  };
                  ease = lib.mkOption {
                    type = cfg_ease;
                  };
                  style = lib.mkOption {
                    type = lib.types.submodule {
                      options = {
                        name = lib.mkOption {
                          type = lib.types.str;
                        };
                        perc = lib.mkOption {
                          type = lib.types.int;
                        };
                        dir = lib.mkOption {
                          type = lib.types.str;
                        };
                      };
                    };
                  };
                };
                config = lib.mkIf (config.spring != null && config.ease != null) {
                  warnings = [ "You cant have both 'spring' and 'ease'" ];
                };
              });
          in
          {
            options.carrot = {
              enable = lib.mkEnableOption "Carrot, a pure Rust wayland compositor";
              settings = mkOption {
                type = types.suboption {
                  options = {
                    binds = mkOption {
                      type = types.listOf (types.suboption {
                        options = {
                          chord = mkOption {
                            type = types.str;
                          };
                          actions = mkOption {
                            type = types.enum actions;
                          };
                          args = mkOption {
                            type = types.listOf types.str;
                            default = {};
                          };
                          on = mkOption {
                            type = types.enum [ "press" "release" ];
                            default = "press";
                          };
                          repeat = mkOption {
                            type = types.bool;
                            default = false;
                          };
                          allow_when_locked = mkOption {
                            type = types.bool;
                            default = false;
                          };
                          cooldown_ms = mkOption {
                            type = types.numbers.between 1 60000;
                            default = null;
                          };
                        };
                      });
                    };
                    input = mkOption {
                      type = types.submodule {
                        options = {
                          keyboard = mkOption {
                            type = types.submodule {
                              options = {
                                xkb = mkOption {
                                  type = types.submodule {
                                    options = {
                                      layout = mkOption {
                                        type = types.str;
                                      };
                                      variant = mkOption {
                                        type = types.str;
                                      };
                                      options = mkOption {
                                        type = types.str;
                                      };
                                    };
                                  };
                                };
                                repeat_rate = mkOption {
                                  type = types.ints.between 1 200;
                                };
                                repeat_delay = mkOption {
                                  type = types.ints.between 1 60000;
                                };
                                numlock = mkOption {
                                  type = types.bool;
                                };
                              };
                            };
                          };  
                          touchpad = mkOption {
                            type = types.submodule {
                              options = {
                                accel_profile = mkOption {
                                  type = types.string;
                                };
                                accel_speed = mkOption {
                                  type = types.numbers.between -1.0 1.0;
                                };
                                natural_scroll = mkOption {
                                  type = types.bool;
                                };
                              };
                            };
                          };
                          mouse = mkOption {
                            type = types.submodule {
                              options = {
                                accel_profile = mkOption {
                                  type = types.string;
                                };
                                accel_speed = mkOption {
                                  type = types.numbers.between -1.0 1.0;
                                };
                                natural_scroll = mkOption {
                                  type = types.bool;
                                };
                              };
                            };
                          };
                          devices = mkOption {
                            type = types.listOf (types.submodule {
                              options = {
                                accel_speed = mkOption {
                                  type = types.numbers.between -1.0 1.0;
                                };
                                accel_profile = mkOption {
                                  type = types.str;
                                };
                                natural_scroll = mkOption {
                                  type = types.bool;
                                };
                                dpi = mkOption {
                                  type = types.numbers.between 100 40000;
                                };
                              };
                            });
                          };
                          mod_key = mkOption {
                            type = types.str;
                          };
                        };
                      };
                    };
                    window_rules = mkOption {
                      type = types.listOf (types.submodule {
                        options = {
                          match = mkOption {
                            type = types.str;
                          };
                          exclude = mkOption {
                            type = types.str;
                          };
                          open_floating = mkOption {
                            type = types.bool;
                          };
                          open_on_workspace = mkOption {
                            type = types.ints.positive;
                          };
                          default_size = mkOption {
                            type = types.listOf types.int;
                          };
                          open_centered = mkOption {
                            type = types.bool;
                          };
                          opacity = mkOption {
                            type = types.numbers.between 0.0 1.0;
                          };
                          allow_tearing = mkOption {
                            type = types.bool;
                          };
                          no_anim = mkOption {
                            type = types.bool;
                          };
                          rounding = mkOption {
                            type = types.ints.between 0 200;
                          };
                          shadow = mkOption {
                            type = types.bool;
                          };
                          dim = mkOption {
                            type = types.bool;
                          };
                        };
                      });
                    };
                    spawn_at_startup = mkOption {
                      type = types.listOf types.str;
                    };
                    animations = mkOption {
                      type = types.listOf (types.submodule {
                        options = {
                          off = mkOption {
                            type = types.bool;
                          };
                          slowdown = mkOption {
                            type = types.numbers.between 0.1 10.0;
                          };
                          curves = mkOption {
                            type = types.attrsOf (types.listOf types.number);
                          };
                          spring = mkOption {
                            type = cfg_spring;
                          };
                          ease = mkOption {
                            type = cfg_ease;
                          };
                          window_open = mkOption {
                            type = cfg_anim_kind;
                          };
                          window_close = mkOption {
                            type = cfg_anim_kind;
                          };
                          window_move = mkOption {
                            type = cfg_anim_kind;
                          };
                          window_resize = mkOption {
                            type = cfg_anim_kind;
                          };
                          workspace_switch = mkOption {
                            type = cfg_anim_kind;
                          };
                          view_movement = mkOption {
                            type = cfg_anim_kind;
                          };
                          layer_open = mkOption {
                            type = cfg_anim_kind;
                          };
                          layer_close = mkOption {
                            type = cfg_anim_kind;
                          };
                          border_color = mkOption {
                            type = cfg_anim_kind;
                          };
                        };
                      });
                    };
                    decoration = mkOption {
                      type = types.submodule {
                        options = {
                          rounding = mkOption {
                            type = types.ints.between 0 200;
                          };
                          rounding_power = mkOption {
                            type = types.numbers.between 1.0 8.0;
                          };
                          dim_inactive = mkOption {
                            type = types.numbers.between 0.0 1.0;
                          };
                          shadow = mkOption {
                            type = types.submodule {
                              options = {
                                size = mkOption {
                                  type = types.ints.between 1 200;
                                };
                                color = mkOption {
                                  type = types.str;
                                };
                                offset = mkOption {
                                  type = types.listOf types.ints.between -500 500;
                                };
                                power = mkOption {
                                  type = types.numbers.between 0.5 8.0;
                                };
                              };
                            };
                          };
                        };
                      };
                    };
                    layout = mkOption {
                      type = types.submodule {
                        options = {
                          mode = mkOption {
                            type = types.enum [ "scrolling" "dwindle" ];
                          };
                          scrolling = {
                            type = types.submodule {
                              options = {
                                preset_widths = mkOption {
                                  type = types.listOf types.numbers.between 0.05 1.0;
                                };
                                default_width = mkOption {
                                  type = types.numbers.between 0.05 1.0;
                                };
                                default_width_px = mkOption {
                                  type = types.numbers.between 50 100000;
                                };
                                center_focus = mkOption {
                                  type = types.enum ["never" "always" "on-overflow"];
                                };
                              };
                            };
                          };
                          gaps_in = mkOption {
                            type = types.ints.between 0 500;
                          };
                          gaps_out = mkOption {
                            type = types.ints.between 0 500;
                          };
                          border = mkOption {
                            type = types.submodule {
                              options = {
                                width = mkOption {
                                  type = types.ints.between 0 100;
                                };
                                active_color = mkOption {
                                  type = types.str;
                                };
                                inactive_color = mkOption {
                                  type = types.str;
                                };
                              };
                            };
                          };
                          float_above_fullscreen = mkOption {
                            type = types.bool;
                          };
                        };
                      };
                    };
                    outputs = mkOption {
                      type = types.attrsOf (types.submodule {
                        options = {
                          mode = mkOption {
                            type = types.str;
                          };
                          scale = mkOption {
                            type = types.numbers.between 0.25 4.0;
                          };
                          position = mkOption {
                            type = types.submodule {
                              options = {
                                x = mkOption {
                                  type = types.int;
                                };
                                y = mkOption {
                                  type = types.int;
                                };
                              };
                            };
                          };
                          vrr = mkOption {
                            type = types.enum ["off" "on-demand" "always"];
                          };
                          off = mkOption {
                            type = types.bool;
                          };
                          allow_tearing = mkOption {
                            type = types.bool;
                          };
                        };
                      });
                    };
                    cursor = mkOption {
                      type = types.submodule {
                        options = {
                          xcursor_theme = mkOption {
                            type = types.str;
                          };
                          xcursor_size = mkOption {
                            type = types.ints.between 1 512;
                          };
                          software = mkOption {
                            type = types.bool;
                          };
                        };
                      };
                    };
                    environment = mkOption {
                      type = types.attrsOf (types.either types.str (types.enum [ false ]));
                    };
                    prefer_no_csd = mkOption {
                      type = types.bool;
                    };
                    screencast = mkOption {
                      type = types.submodule {
                        options = {
                          picker = mkOption {
                            type = types.str;
                          };
                        };
                      };
                    };
                    debug = mkOption {
                      type = types.submodule {
                        render_drm_device = mkOption {
                          type = types.str;
                        };
                        ignore_drm_device = mkOption {
                          type = types.str;
                        };
                      };
                    };
                    remaps = mkOption {
                      type = types.listOf (types.submodule {
                        options = {
                          name = mkOption {
                            type = types.str;
                          };
                          match = mkOption {
                            type = types.submodule {
                              options = {
                                app_id = mkOption {
                                  type = types.str;
                                };
                                title = mkOption {
                                  type = types.str;
                                };
                                is_xwayland = mkOption {
                                  type = types.bool;
                                };
                                pid = mkOption {
                                  type = types.int;
                                };
                                workspace = mkOption {
                                  type = types.int;
                                };
                              };
                            };
                          };
                          maps = mkOption {
                            type = types.listOf (types.listOf lib.types.str);
                          };
                        };
                      });
                    };
                  };
                };
              };
            };
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
