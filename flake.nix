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
        homeManagerModules =
          { config, lib, pkgs, ... }:
          let
            inherit (lib)
              mkIf
              mkOption
              types
              ;
            cfg = config.carrot;
            actions = [
              "spawn-sh" "spawn"
              "focus-workspace" "move-to-workspace" "send-to-workspace"
              "close-window"
              "toggle-fullscreen" "toggle-floating"
              "focus-prev" "focus-next"
              "focus-left" "focus-right" "focus-down" "focus-up"
              "swap-left" "swap-right" "swap-down" "swap-up"
              "adjust-split-ratio"
              "consume-or-expel-left" "consume-or-expel-right"
              "move-column-left" "move-column-right"
              "cycle-column-width" "cycle-column-width-back" "toggle-full-width"
              "center-column"
              "pointer-move" "pointer-resize"
              "set-layout"
              "quit"
            ];
            cfg_spring = types.submodule {
              options = {
                damping_ratio = mkOption {
                  type = types.nullOr types.number;
                  default = null;
                };
                stiffness = mkOption {
                  type = types.nullOr types.number;
                  default = null;
                };
                epsilon = mkOption {
                  type = types.nullOr types.number;
                  default = null;
                };
              };
            };
            cfg_ease = types.submodule {
              options = {
                duration_ms = mkOption {
                  type = types.nullOr types.int;
                  default = null;
                };
                curve = mkOption {
                  type = types.nullOr types.str;
                  default = null;
                };
              };
            };
            cfg_anim_kind = types.submodule {
              options = {
                off = mkOption {
                  type = types.nullOr types.bool;
                  default = null;
                };
                spring = mkOption {
                  type = types.nullOr cfg_spring;
                  default = null;
                };
                ease = mkOption {
                  type = types.nullOr cfg_ease;
                  default = null;
                };
                style = mkOption {
                  type = types.nullOr (types.submodule {
                    options = {
                      name = mkOption {
                        type = types.nullOr types.str;
                        default = null;
                      };
                      perc = mkOption {
                        type = types.nullOr types.int;
                        default = null;
                      };
                      dir = mkOption {
                        type = types.nullOr types.str;
                        default = null;
                      };
                    };
                  });
                  default = null;
                };
              };
            };
            matcher = types.submodule {
              options = {
                app_id = mkOption {
                  type = types.nullOr types.str;
                  default = null;
                };
                title = mkOption {
                  type = types.nullOr types.str;
                  default = null;
                };
                is_fullscreen = mkOption {
                  type = types.nullOr types.bool;
                  default = null;
                };
                is_floating = mkOption {
                  type = types.nullOr types.bool;
                  default = null;
                };
                is_xwayland = mkOption {
                  type = types.nullOr types.bool;
                  default = null;
                };
              };
            };
          in
          {
            options.wayland.windowManager.carrot = {
              enable = lib.mkEnableOption "Carrot, a pure Rust wayland compositor";
              package = mkOption {
                type = types.nullOr types.package;
                default = inputs.self.packages.${pkgs.stdenv.hostPlatform.system}.carrot;
                defaultText = "inputs.self.packages.${pkgs.stdenv.hostPlatform.system}.carrot";
              };
              settings = mkOption {
                type = types.nullOr (types.submodule {
                  options = {
                    binds = mkOption {
                      type = types.nullOr (types.listOf (types.submodule {
                        options = {
                          chord = mkOption {
                            type = types.nullOr types.str;
                            default = null;
                          };
                          action = mkOption {
                            type = types.nullOr (types.enum actions);
                            default = null;
                          };
                          args = mkOption {
                            type = types.nullOr (types.listOf (types.either types.str types.number));
                            default = null;
                          };
                          on = mkOption {
                            type = types.nullOr (types.enum [ "press" "release" ]);
                            default = null;
                          };
                          repeat = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                          allow_when_locked = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                          cooldown_ms = mkOption {
                            type = types.nullOr (types.numbers.between 1 60000);
                            default = null;
                          };
                        };
                      }));
                      default = null;
                    };
                    input = mkOption {
                      type = types.nullOr (types.submodule {
                        options = {
                          keyboard = mkOption {
                            type = types.nullOr (types.submodule {
                              options = {
                                xkb = mkOption {
                                  type = types.nullOr (types.submodule {
                                    options = {
                                      layout = mkOption {
                                        type = types.nullOr types.str;
                                        default = null;
                                      };
                                      variant = mkOption {
                                        type = types.nullOr types.str;
                                        default = null;
                                      };
                                      options = mkOption {
                                        type = types.nullOr types.str;
                                        default = null;
                                      };
                                    };
                                  });
                                  default = null;
                                };
                                repeat_rate = mkOption {
                                  type = types.nullOr (types.ints.between 1 200);
                                  default = null;
                                };
                                repeat_delay = mkOption {
                                  type = types.nullOr (types.ints.between 1 60000);
                                  default = null;
                                };
                                numlock = mkOption {
                                  type = types.nullOr types.bool;
                                  default = null;
                                };
                              };
                            });
                            default = null;
                          };
                          touchpad = mkOption {
                            type = types.nullOr (types.submodule {
                              options = {
                                accel_profile = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                accel_speed = mkOption {
                                  type = types.nullOr (types.numbers.between (-1.0) 1.0);
                                  default = null;
                                };
                                natural_scroll = mkOption {
                                  type = types.nullOr types.bool;
                                  default = null;
                                };
                              };
                            });
                            default = null;
                          };
                          mouse = mkOption {
                            type = types.nullOr (types.submodule {
                              options = {
                                accel_profile = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                accel_speed = mkOption {
                                  type = types.nullOr (types.numbers.between (-1.0) 1.0);
                                  default = null;
                                };
                                natural_scroll = mkOption {
                                  type = types.nullOr types.bool;
                                  default = null;
                                };
                              };
                            });
                            default = null;
                          };
                          devices = mkOption {
                            type = types.nullOr (types.attrsOf (types.submodule {
                              options = {
                                accel_speed = mkOption {
                                  type = types.nullOr (types.numbers.between (-1.0) 1.0);
                                  default = null;
                                };
                                accel_profile = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                natural_scroll = mkOption {
                                  type = types.nullOr types.bool;
                                  default = null;
                                };
                                dpi = mkOption {
                                  type = types.nullOr (types.numbers.between 100 40000);
                                  default = null;
                                };
                              };
                            }));
                            default = null;
                          };
                          mod_key = mkOption {
                            type = types.nullOr types.str;
                            default = null;
                          };
                        };
                      });
                      default = null;
                    };
                    window_rules = mkOption {
                      type = types.nullOr (types.listOf (types.submodule {
                        options = {
                          match = mkOption {
                            type = types.nullOr (types.listOf matcher);
                            default = null;
                          };
                          exclude = mkOption {
                            type = types.nullOr (types.listOf matcher);
                            default = null;
                          };
                          open_floating = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                          open_on_workspace = mkOption {
                            type = types.nullOr types.ints.positive;
                            default = null;
                          };
                          default_size = mkOption {
                            type = types.nullOr (types.listOf types.int);
                            default = null;
                          };
                          open_centered = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                          opacity = mkOption {
                            type = types.nullOr (types.numbers.between 0.0 1.0);
                            default = null;
                          };
                          allow_tearing = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                          no_anim = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                          rounding = mkOption {
                            type = types.nullOr (types.ints.between 0 200);
                            default = null;
                          };
                          shadow = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                          dim = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                        };
                      }));
                      default = null;
                    };
                    spawn_at_startup = mkOption {
                      type = types.nullOr (types.listOf types.str);
                      default = null;
                    };
                    animations = mkOption {
                      type = types.nullOr (types.submodule {
                        options = {
                          off = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                          slowdown = mkOption {
                            type = types.nullOr (types.numbers.between 0.1 10.0);
                            default = null;
                          };
                          curves = mkOption {
                            type = types.nullOr (types.attrsOf (types.listOf types.number));
                            default = null;
                          };
                          spring = mkOption {
                            type = types.nullOr cfg_spring;
                            default = null;
                          };
                          ease = mkOption {
                            type = types.nullOr cfg_ease;
                            default = null;
                          };
                          window_open = mkOption {
                            type = types.nullOr cfg_anim_kind;
                            default = null;
                          };
                          window_close = mkOption {
                            type = types.nullOr cfg_anim_kind;
                            default = null;
                          };
                          window_move = mkOption {
                            type = types.nullOr cfg_anim_kind;
                            default = null;
                          };
                          window_resize = mkOption {
                            type = types.nullOr cfg_anim_kind;
                            default = null;
                          };
                          workspace_switch = mkOption {
                            type = types.nullOr cfg_anim_kind;
                            default = null;
                          };
                          view_movement = mkOption {
                            type = types.nullOr cfg_anim_kind;
                            default = null;
                          };
                          layer_open = mkOption {
                            type = types.nullOr cfg_anim_kind;
                            default = null;
                          };
                          layer_close = mkOption {
                            type = types.nullOr cfg_anim_kind;
                            default = null;
                          };
                          border_color = mkOption {
                            type = types.nullOr cfg_anim_kind;
                            default = null;
                          };
                        };
                      });
                      default = null;
                    };
                    decoration = mkOption {
                      type = types.nullOr (types.submodule {
                        options = {
                          rounding = mkOption {
                            type = types.nullOr (types.ints.between 0 200);
                            default = null;
                          };
                          rounding_power = mkOption {
                            type = types.nullOr (types.numbers.between 1.0 8.0);
                            default = null;
                          };
                          dim_inactive = mkOption {
                            type = types.nullOr (types.numbers.between 0.0 1.0);
                            default = null;
                          };
                          shadow = mkOption {
                            type = types.nullOr (types.submodule {
                              options = {
                                size = mkOption {
                                  type = types.nullOr (types.ints.between 1 200);
                                  default = null;
                                };
                                color = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                offset = mkOption {
                                  type = types.nullOr (types.listOf (types.ints.between (-500) 500));
                                  default = null;
                                };
                                power = mkOption {
                                  type = types.nullOr (types.numbers.between 0.5 8.0);
                                  default = null;
                                };
                              };
                            });
                            default = null;
                          };
                        };
                      });
                      default = null;
                    };
                    layout = mkOption {
                      type = types.nullOr (types.submodule {
                        options = {
                          mode = mkOption {
                            type = types.nullOr (types.enum [ "scrolling" "dwindle" ]);
                            default = null;
                          };
                          workspace_axis = mkOption {
                            type = types.nullOr (types.enum [ "vertical" "horizontal" ]);
                            default = null;
                          };
                          scrolling = mkOption {
                            type = types.nullOr (types.submodule {
                              options = {
                                preset_widths = mkOption {
                                  type = types.nullOr (types.listOf (types.numbers.between 0.05 1.0));
                                  default = null;
                                };
                                default_width = mkOption {
                                  type = types.nullOr (types.numbers.between 0.05 1.0);
                                  default = null;
                                };
                                default_width_px = mkOption {
                                  type = types.nullOr (types.numbers.between 50 100000);
                                  default = null;
                                };
                                center_focus = mkOption {
                                  type = types.nullOr (types.enum ["never" "always" "on-overflow"]);
                                  default = null;
                                };
                              };
                            });
                            default = null;
                          };
                          gaps_in = mkOption {
                            type = types.nullOr (types.ints.between 0 500);
                            default = null;
                          };
                          gaps_out = mkOption {
                            type = types.nullOr (types.ints.between 0 500);
                            default = null;
                          };
                          border = mkOption {
                            type = types.nullOr (types.submodule {
                              options = {
                                width = mkOption {
                                  type = types.nullOr (types.ints.between 0 100);
                                  default = null;
                                };
                                active_color = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                inactive_color = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                              };
                            });
                            default = null;
                          };
                          float_above_fullscreen = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                        };
                      });
                      default = null;
                    };
                    outputs = mkOption {
                      type = types.nullOr (types.attrsOf (types.submodule {
                        options = {
                          mode = mkOption {
                            type = types.nullOr types.str;
                            default = null;
                          };
                          scale = mkOption {
                            type = types.nullOr (types.numbers.between 0.25 4.0);
                            default = null;
                          };
                          position = mkOption {
                            type = types.nullOr (types.submodule {
                              options = {
                                x = mkOption {
                                  type = types.nullOr types.int;
                                  default = null;
                                };
                                y = mkOption {
                                  type = types.nullOr types.int;
                                  default = null;
                                };
                              };
                            });
                            default = null;
                          };
                          vrr = mkOption {
                            type = types.nullOr (types.enum ["off" "on-demand" "always"]);
                            default = null;
                          };
                          off = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                          allow_tearing = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                        };
                      }));
                      default = null;
                    };
                    cursor = mkOption {
                      type = types.nullOr (types.submodule {
                        options = {
                          xcursor_theme = mkOption {
                            type = types.nullOr types.str;
                            default = null;
                          };
                          xcursor_size = mkOption {
                            type = types.nullOr (types.ints.between 1 512);
                            default = null;
                          };
                          software = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                        };
                      });
                      default = null;
                    };
                    environment = mkOption {
                      type = types.nullOr (types.attrsOf (types.either types.str (types.enum [ false ])));
                      default = null;
                    };
                    prefer_no_csd = mkOption {
                      type = types.nullOr types.bool;
                      default = null;
                    };
                    screencast = mkOption {
                      type = types.nullOr (types.submodule {
                        options = {
                          picker = mkOption {
                            type = types.nullOr types.str;
                            default = null;
                          };
                        };
                      });
                      default = null;
                    };
                    debug = mkOption {
                      type = types.nullOr (types.submodule {
                        options = {
                          render_drm_device = mkOption {
                            type = types.nullOr types.str;
                            default = null;
                          };
                          ignore_drm_devices = mkOption {
                            type = types.nullOr types.str;
                            default = null;
                          };
                        };
                      });
                      default = null;
                    };
                    remaps = mkOption {
                      type = types.nullOr (types.listOf (types.submodule {
                        options = {
                          name = mkOption {
                            type = types.nullOr types.str;
                            default = null;
                          };
                          match = mkOption {
                            type = types.nullOr (types.submodule {
                              options = {
                                app_id = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                title = mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                is_xwayland = mkOption {
                                  type = types.nullOr types.bool;
                                  default = null;
                                };
                                pid = mkOption {
                                  type = types.nullOr types.int;
                                  default = null;
                                };
                                workspace = mkOption {
                                  type = types.nullOr types.int;
                                  default = null;
                                };
                              };
                            });
                            default = null;
                          };
                          maps = mkOption {
                            type = types.nullOr (types.listOf (types.listOf types.str));
                            default = null;
                          };
                        };
                      }));
                      default = null;
                    };
                  };
                });
                default = null;
              };
            };
            config = mkIf cfg.enable {
              home.packages = [ cfg.package ];

              xdg.configFile."carrot/carrot.lua" = mkIf (cfg.settings != {}) {
                text = let
                  luaConfig = lib.generators.toLua { } cfg.settings;
                in
                  ''
                    carrot = ${luaConfig}
                  '';
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

          src = lib.cleanSourceWith {
            src = ./.;
            filter = craneLib.filterCargoSources;
          };

          commonArgs = {
            inherit src;
            pname = "carrot";
            version = "0.1.0";
            strictDeps = true;

            nativeBuildInputs = [ pkgs.makeWrapper ];

            XKB_CONFIG_ROOT = "${pkgs.xkeyboard-config}/share/X11/xkb";
          };

          carrot = craneLib.buildPackage (commonArgs // {
            cargoArtifacts = craneLib.buildDepsOnly commonArgs;

            postInstall = ''
              wrapProgram $out/bin/carrot \
                --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [ pkgs.vulkan-loader ]} \
                --set-default XKB_CONFIG_ROOT ${pkgs.xkeyboard-config}/share/X11/xkb

              mkdir -p $out/share/wayland-sessions
              cat > $out/share/wayland-sessions/carrot.desktop << EOF
              [Desktop Entry]
              Name=Carrot
              Comment=A pure Rust tiling Wayland compositor
              Exec=$out/bin/carrot
              Type=Application
              DesktopNames=carrot
              EOF

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
              binutils

              vulkan-tools
              vulkan-validation-layers
              renderdoc

              wev
              wayland-utils
            ];

            env = {
              LD_LIBRARY_PATH = lib.makeLibraryPath [ pkgs.vulkan-loader ];
              VK_LAYER_PATH = "${pkgs.vulkan-validation-layers}/share/vulkan/explicit_layer.d";
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
