# Changelog

## Carrot v0.1.3 Beta

Input correctness across games, grabs and workspaces, one new protocol.

- Present: joined modes ride the cursor on an overlay plane, so
  fullscreen dmabuf scans out directly again at >1GHz pixel clocks
- Present: a callback sweep that slips past the next flip coalesces
  into it instead of firing twice; presentation stops reporting
  discarded frames
- Present: replaced dmabufs release after the frames that sampled
  them, not at gpu idle; electron apps no longer hang at startup
- Input: cursor commits wait for an idle screen, so a high-rate mouse
  no longer starves the present loop
- Input: the pointer origin heals under any grab; fullscreen under a
  locked pointer keeps clicks where the cursor is
- xwayland: surface pairing follows the serial across map cycles, so
  hidden apps come back clickable
- xwayland: fullscreen windows answer configure requests with the
  painted rect instead of the layout tile; game clicks land where the
  cursor is, on every output
- xwayland: hidden workspaces iconify their windows for real
  (WM_STATE), so a buried menu's pointer grab releases the seat
  instead of eating clicks across workspaces
- Protocol: ext-workspace v1 for pagers and docks; groups follow
  outputs, workspaces diff atomically against what each client was
  last told, and activate, assign and create are backed by real
  mutators
- config: a move-workspace-to-output bind action, the first
  workspace-to-output mutator

## Carrot v0.1.1 Beta

Hardening across the launch path, no new features.

- Loader: packed relative relocations (DT_RELR) load on distros that
  build mesa with them; the residual unaligned entries no longer fail
  the whole driver dlopen
- Loader: multilib systems pick the 64-bit driver; foreign-arch ICD
  manifests are skipped and every matching ICD gets a fallback try
- Loader: qemu guests match the venus ICD (virtio_gpu)
- Loader: the taproot libc pairing is verified before any driver code
  runs, on libc.so.6 and libm.so.6 both; a missing legacy-soname stub
  is a hard error instead of a silent glibc leak
- taproot: the thread metadata prefix is frozen repr(C), so a cdylib
  built by a different compiler can no longer corrupt the session
- taproot: the cdylib links clean under GNU ld (the init/fini array
  bounds no longer become unresolvable imports)
- install: the libc family stages all-or-nothing, verified against the
  installing binary, stale files swept, writes are atomic
- install: a udev rule grants the active seat /dev/udmabuf, so the
  zero-copy shm path works out of the box
- install: --build-taproot makes a cargo install GPU-capable in one
  command; it fetches the matching taproot source with curl and builds
  the libc family with your own cargo, pairing-checked before staging
- config: a multibyte character in a color value errors instead of
  crashing the compositor at startup

## Carrot v0.1.0 Beta

- Dwindle Tiling
- Workspaces
- Window Borders
- Window Gaps
- Fullscreen
- Fullscreen Borderless
- Cursors
- Cursor Warping
- Complete XWayland Client
- Complete Input Stack
- Built In Rebinds Per Window
- Per Input Device Configs
- Complete Vulkan Graphics Pipeline
- Burrow IPC
- VT Switching
- DMA-BUF Import with Explicit Sync
- Double Buffered Output
- Hardware Cursor Plane
- Screenshot Tool Compatiblity
- Clipboard
- Device Hotplug
- Pointer Locking
- XKB Keyboard Layouts
- EI Input Injection Server
- Logind Session Integration
- KDL Configuration
- Lua Configuration
- Multi Monitor Support
- Monitor Hotplug
- Layer Shell Support
- Drag and Drop
- Tearing
- Adaptive Sync (VRR)
- Taskbar & Dock Support
- Clipboard Manager Support
- Screen Recording Support
- Window Rules
- Launch to Workspace
- Per Window Opacity
- Interactive Move & Resize
- Split Ratio Control
- Relative Workspace Navigation
- Directional Focus & Swap
- Floating Windows
- Idle Management
- Idle Inhibit
- Screen Sleep & Wake on Input (DPMS)
- Game Input (Relative Pointer & Constraints)
- App Launcher & Widget Keyboard Support
- Lock Screen (ext-session-lock)
- PipeWire Screencast Portal
- Pure Rust PipeWire Client
- Window, Workspace & Output Casting
- Hidden Workspace Casting
- Screenshare Restore Tokens
- Shell Agnostic Share Picker
- Presentation Time (wp_presentation)
- Animations (Window Open/Close/Move, Workspace Switch, Layer Surfaces, Border Color)
- Per-Kind Animation Config (Springs, Easings, Custom Bezier Curves, Styles)
- Animation Clock Locked to Predicted Presentation Time
- Scrolling Layout (Per-Workspace Columns, Animated View, Width Presets)
- Runtime Layout Switching (set-layout, Vertical Workspace Axis Rule)
- Rounded Corners (SDF-Clipped Sampling, Ring Borders)
- Drop Shadows (Distance Falloff, Body Cutout)
- Dim Inactive Windows (Animated)
- Resize Crossfade (Old and New Content Mix Across the Animated Geometry)
- Offscreen Sampled Render Targets
- Pointer Move/Resize Actions (Key or Mouse-Chord Grabs)
- Kawase Blur (Backdrop Cache, Per-Window and Per-Layer Rules)
- Tiled Drag-and-Swap (Pointer Grabs Trade Window Slots, Cross-Output on Dwindle)
- Alpha-Masked Layer Blur (ignore-alpha Layer Rule, Backdrop Clips to the Surface's Own Coverage)
- No-Capture Window Rule (Screenshares, Recordings & Screenshots See a Black Stand-In)
- No-Anim Layer Rule (Shells That Remap Layers Skip Open/Close Styles)
- Live Rule Reload (Config Edits Land on Running Windows)
- Single-Submit Frames (Offscreen Work Records as Ordered Pre-Passes, No Blocking GPU Waits)
- Display Manager Sessions on Any Distro (carrot install: Session Entry, Portal Registration, IPC Client)
- XDG Activation (Link Handoffs Focus the Running App and Follow It to Its Workspace)
- Multi-File Configs (KDL include Nodes and a Lua include(), Paths Resolve Against the Including File)
- Workspace Axis Choice (Dwindle Picks Horizontal or Vertical Switching; Scrolling Stays Vertical)
- Move-Column Verbs (The Whole Column Leapfrogs the Strip; Directional Swap Trades Window Slots Between Columns)
- Guided Default Config (Per-Section Walkthrough, Decoration & No-CSD On, Screenshot/Media/Brightness Binds, Vim Keys, include Examples)
- Numbered Crash Reports (Panic + Backtrace + stderr Tail in ~/.cache/carrot/carrotCrashLogN.log, Nothing Overwritten; the /tmp Log Retired)
- Pinned Nightly Toolchain (rust-toolchain.toml Matches the Flake and taproot)
- AMD/radv Sessions (taproot Recursive-Mutex ABI Fix Unwedges libLLVM Init; Stub Sonames Keep glibc Out of Driver Closures)
- carrot doctor (One Run Reports Every GPU Bring-Up Stage + glibc-Leak Sweep to ~/carrotDoctor.log; Full Stub Family Covers libutil/libresolv)
- NVIDIA Sessions (Vendored dlopen-rs Routes the Driver's Own dl* Calls, Survives Recursive dlopen + Versioned Lookups; Driver Threads Get Real TLS)
- Monotonic Input Timestamps (EVIOCSCLOCKID at Open + vt Resume; Device and Synthetic Events Share One Clock)
- Late-Latch Frame Scheduling (Dirty Frames Render Just Ahead of Their Vblank Under an Adaptive Margin; Frame Callbacks Fire at Latch for Every-Vblank Client Pacing)
- Fullscreen Direct Scanout (A Lone dmabuf Rides the Primary Plane With Zero Compositor GPU Work; ZERO_COPY Presentation Feedback)
