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

    # the libc family the gpu driver binds at runtime; built from the
    # workspace, never the registry: the cdylib's export set depends on
    # the link shim and profile pin that only live in the repo
    taproot = {
      url = "github:carrot-wm/taproot";
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
            # the package ships 60-carrot-udmabuf.rules; udev.packages keeps
            # the filename, so it still sorts before 70-uaccess.rules (the
            # applier). extraRules would land in 99-local.rules - too late
            services.udev.packages = [ package ];
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

      flake.homeManagerModules.default =
          { config, lib, pkgs, ... }:
          let
            inherit (lib)
              mkIf
              mkOption
              types
              ;
            cfg = config.wayland.windowManager.carrot;
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
              "focus-column-first" "focus-column-last"
              "move-column-to-first" "move-column-to-last"
              "consume-window-into-column" "expel-window-from-column"
              "expand-column-to-available-width"
              "cycle-window-height" "cycle-window-height-back" "reset-window-height"
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
                        type = types.nullOr (types.enum [ "popin" "fade" "slide" "slidevert" "slidefade" "slidefadevert" ]);
                        default = null;
                      };
                      perc = mkOption {
                        type = types.nullOr types.int;
                        default = null;
                      };
                      dir = mkOption {
                        type = types.nullOr (types.enum [ "top" "bottom" "left" "right" ]);
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

              # home-manager's way-displays module assigns systemd.variables to
              # every wayland.windowManager.* entry (it mapAttrs over
              # options.wayland.windowManager), and a definition for an
              # undeclared option is fatal, so these must exist even though
              # carrot does not implement session-target integration yet.
              systemd = {
                enable = mkOption {
                  type = types.bool;
                  default = false;
                  example = true;
                  description = ''
                    Whether to enable systemd session integration for carrot.

                    Currently inert: this option only exists so home-manager
                    modules built on the `wayland.windowManager.<wm>.systemd`
                    convention (e.g. way-displays) don't fail to evaluate. It
                    will take effect once carrot gains session-target support.
                  '';
                };
                variables = mkOption {
                  type = types.listOf types.str;
                  default = [ ];
                  example = [ "XDG_SESSION_TYPE" ];
                  description = ''
                    Extra variables to import into the systemd and D-Bus user
                    environment, on top of the {env}`WAYLAND_DISPLAY`,
                    {env}`DISPLAY` and {env}`XDG_CURRENT_DESKTOP` that carrot
                    already imports.

                    Currently inert, pending session-target support in carrot.
                  '';
                };
                extraCommands = mkOption {
                  type = types.listOf types.str;
                  default = [ ];
                  example = [ "systemctl --user start carrot-session.target" ];
                  description = ''
                    Commands to run once the session target is started.

                    Currently inert, pending session-target support in carrot.
                  '';
                };
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
                          title = mkOption {
                            type = types.nullOr types.str;
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
                                  type = types.nullOr (types.enum [ "flat" "adaptive" ]);
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
                                  type = types.nullOr (types.enum [ "flat" "adaptive" ]);
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
                                  type = types.nullOr (types.enum [ "flat" "adaptive" ]);
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
                            type = types.nullOr (types.enum [ "super" "alt" ]);
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
                          no_capture = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                          animation = mkOption {
                            type = types.nullOr types.str;
                            default = null;
                          };
                          blur = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                        };
                      }));
                      default = null;
                    };
                    layer_rules = mkOption {
                      type = types.nullOr (types.listOf (types.submodule {
                        options = {
                          match = mkOption {
                            type = types.nullOr (types.listOf types.str);
                            default = null;
                          };
                          blur = mkOption {
                            type = types.nullOr types.bool;
                            default = null;
                          };
                          ignore_alpha = mkOption {
                            type = types.nullOr (types.numbers.between 0.0 1.0);
                            default = null;
                          };
                          no_anim = mkOption {
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
                                  type = types.nullOr (types.strMatching "#([0-9a-fA-F]{3,4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})");
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
                          blur = mkOption {
                            type = types.nullOr (types.submodule {
                              options = {
                                passes = mkOption {
                                  type = types.nullOr (types.ints.between 1 4);
                                  default = null;
                                };
                                size = mkOption {
                                  type = types.nullOr (types.numbers.between 0.5 6.0);
                                  default = null;
                                };
                                noise = mkOption {
                                  type = types.nullOr (types.numbers.between 0.0 1.0);
                                  default = null;
                                };
                                contrast = mkOption {
                                  type = types.nullOr (types.numbers.between 0.0 2.0);
                                  default = null;
                                };
                                brightness = mkOption {
                                  type = types.nullOr (types.numbers.between 0.0 2.0);
                                  default = null;
                                };
                                xray = mkOption {
                                  type = types.nullOr types.bool;
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
                                # widths above 1.0 are columns wider than the output
                                preset_widths = mkOption {
                                  type = types.nullOr (types.listOf (types.numbers.between 0.05 10.0));
                                  default = null;
                                };
                                default_width = mkOption {
                                  type = types.nullOr (types.numbers.between 0.05 10.0);
                                  default = null;
                                };
                                default_width_px = mkOption {
                                  type = types.nullOr (types.numbers.between 50 100000);
                                  default = null;
                                };
                                preset_heights = mkOption {
                                  type = types.nullOr (types.listOf (types.numbers.between 0.05 0.95));
                                  default = null;
                                };
                                center_focus = mkOption {
                                  type = types.nullOr (types.enum ["never" "always" "on-overflow"]);
                                  default = null;
                                };
                                center_single_column = mkOption {
                                  type = types.nullOr types.bool;
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
                                  type = types.nullOr (types.strMatching "#([0-9a-fA-F]{3,4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})");
                                  default = null;
                                };
                                inactive_color = mkOption {
                                  type = types.nullOr (types.strMatching "#([0-9a-fA-F]{3,4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})");
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
                            type = types.nullOr (types.strMatching "[0-9]+x[0-9]+(@[0-9]+)?");
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
                            type = types.nullOr (types.listOf types.str);
                            default = null;
                          };
                          latency_policy = mkOption {
                            type = types.nullOr (types.enum [ "late-latch" "vblank" ]);
                            default = null;
                          };
                          latch_margin_us = mkOption {
                            type = types.nullOr types.ints.unsigned;
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

              xdg.configFile."carrot/carrot.lua" = mkIf (cfg.settings != null) {
                text = let
                  luaConfig = lib.generators.toLua { } cfg.settings;
                in
                  ''
                    carrot = ${luaConfig}
                  '';
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

          # stable: eyra links the precompiled std against taproot's libc
          # symbols (build.rs emits --allow-multiple-definition), so no
          # -Z build-std and no rust-src.
          toolchain =
            inputs'.fenix.packages.stable.withComponents [
              "cargo"
              "rustc"
              "rust-std"
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
            version = "0.1.3";
            strictDeps = true;

            nativeBuildInputs = [ pkgs.makeWrapper ];

            # the keymap tests build real xkb state in the check phase
            XKB_CONFIG_ROOT = "${pkgs.xkeyboard-config}/share/X11/xkb";
          };

          # taproot's libc.so.6/libm.so.6 (full copies of the cdylib) and
          # the legacy-soname stubs a driver closure may name
          taproot-libs = craneLib.buildPackage {
            src = inputs.taproot;
            pname = "taproot-libs";
            # tracks the taproot cdylib crate version; the payload is
            # whatever the flake input pins
            version = "0.22.7";
            strictDeps = true;
            # the c-ward workspace root is a targetless hybrid manifest
            # crane's dummy crate can't model; build in one derivation
            cargoArtifacts = null;
            # the linker shim's env-bash shebang resolves nowhere in the
            # sandbox; point it at the build's own bash
            postPatch = ''
              patchShebangs tools/link-shim.sh
            '';
            cargoExtraArgs = "-p taproot -p taproot-stub";
            doCheck = false;
            nativeBuildInputs = [ pkgs.binutils ];
            installPhaseCommand = ''
              mkdir -p $out/lib
              cp target/release/libtaproot.so $out/lib/libc.so.6
              cp target/release/libtaproot.so $out/lib/libm.so.6
              for s in libpthread.so.0 libdl.so.2 librt.so.1 libutil.so.1 libresolv.so.2 ld-linux-x86-64.so.2; do
                cp target/release/libtaproot_stub.so $out/lib/$s
              done
              # the exports the link shim exists to keep; a miss means it
              # did not run and the session would die at gpu preload
              for sym in memcpy memset memmove malloc free ceil sqrt getauxval _start; do
                nm -D --defined-only $out/lib/libc.so.6 | grep -qw $sym || {
                  echo "libc.so.6 is missing $sym" >&2
                  exit 1
                }
              done
              # and no unresolvable imports: GNU ld defines no
              # __init/fini_array bounds for a shared library, and any
              # strong undefined dynsym here fails preload on loaders
              # without the stub backstop
              if nm -D --undefined-only $out/lib/libc.so.6 | grep ' U '; then
                echo "libc.so.6 has strong undefined imports (above)" >&2
                exit 1
              fi
            '';
          };

          carrot = craneLib.buildPackage (commonArgs // {
            # the shipped binary is panic = "abort"; on stable without
            # build-std cargo builds the test harness panic = "unwind",
            # which needs the unwinder the zero-C design keeps out of
            # production. the suite is validated on the dev shell
            # (build-std + panic-abort-tests) and in CI, not here.
            doCheck = false;

            # crane's dummy crate must link like the real one: libc arrives
            # via `extern crate eyra`, so the stub mains get the same line
            # (and the real build.rs, for its link args) or every libc
            # symbol dangles at the deps-only link
            cargoArtifacts = craneLib.buildDepsOnly (builtins.removeAttrs commonArgs [ "src" ] // {
              dummySrc = craneLib.mkDummySrc {
                inherit src;
                dummyBuildrs = ./build.rs;
                extraDummyScript = ''
                  for f in $out/src/main.rs $out/src/bin/burrow.rs; do
                    chmod +w "$f"
                    printf '\nextern crate eyra;\n' >> "$f"
                  done
                '';
              };
            });

            postInstall = ''
              # the loader looks next to the binary, then ../lib/carrot
              mkdir -p $out/lib/carrot
              cp ${taproot-libs}/lib/* $out/lib/carrot/

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

              # the zero-copy shm bridge opens /dev/udmabuf; uaccess hands it
              # to the active-seat user. 60- so it precedes systemd's
              # 70-uaccess.rules, the rule that applies the tag
              mkdir -p $out/lib/udev/rules.d
              cat > $out/lib/udev/rules.d/60-carrot-udmabuf.rules << EOF
              KERNEL=="udmabuf", TAG+="uaccess"
              EOF
            '';

            passthru.providedSessions = [ "carrot" ];

            meta = {
              description = "A pure Rust tiling Wayland compositor with zero linked C, all the way down to the kernel";
              license = lib.licenses.gpl3;
              platforms = [ "x86_64-linux" ];
              mainProgram = "carrot";
            };
          });
        in
        {
          packages = {
            default = self'.packages.carrot;
            carrot = carrot;
            taproot-libs = taproot-libs;
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
