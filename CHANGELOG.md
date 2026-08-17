# Changelog

## Carrot v0.1.3 Beta

Input correctness across games and grabs, burrow IPC v2, multi-gpu
bring-up, and a capture path that survives its daemons.

**Breaking: the subscribe event format changed.** Events used to be
dispatched by which key was present (`{"window-opened":{...}}`) and carried
only title and app-id. Every event now has an `"event"` key naming it and a
complete record, so a shell never re-queries after one. There is no
compatibility mode. See `doc/ipc.md`.

- IPC: every window event carries the full window record (id, app-id,
  title, pid, workspace, output, column, geometry, state); every workspace
  event carries the full workspace record
- IPC: `window-closed` names the window and the column it was in, captured
  before the tree forgets it
- IPC: `fullscreen` became `window-fullscreen` and now says which window
- IPC: columns are first class. windows carry a stable `column` id that
  survives a strip reorder plus a positional `column-index`, and
  `column-layout-changed` reports strip topology after any verb that moves
  windows between columns or reorders them. both are null on dwindle
- IPC: new `outputs` query with geometry, scale, transform, refresh and
  the layer-shell reserved edges. `scale` and `transform` are honest
  constants: carrot implements neither fractional scale nor rotation yet
- IPC: `outputs-changed` fires on hotplug and whenever an exclusive zone
  moves the reserved area
- IPC: `subscribe --events=windows,columns` filters server side. an event
  no subscriber asked for is never serialized
- IPC: workspace records gained `window-count`; `windows` stays as a
  deprecated alias
- burrow: `workspace`, `workspace +n`, `workspace-rel`, `split-ratio` and
  `spawn` were all sending wire names the Action enum never matched, so
  they silently did nothing. they now send `focus-workspace`,
  `focus-workspace-rel`, `adjust-split-ratio` and an argv array. a test
  pins every verb against the enum so a rename cannot break them again
- doc: `doc/ipc.md` describes every query, record and event
- Protocol: `wp_viewporter`. clients crop and scale a surface's contents:
  a source rectangle picks a region of the buffer, a destination size says
  how big the surface is, and the source is scaled to fill it. both halves
  are double-buffered and land after buffer_transform and buffer_scale, in
  the order the spec fixes. the apply-time errors (bad_size for a
  fractional crop with no destination, out_of_buffer for a rectangle that
  leaves the buffer) are raised at commit, where the buffer they are judged
  against is finally known. a viewported surface stops taking direct
  scanout, since the plane would show the whole buffer 1:1
- Protocol: ext-workspace v1 for pagers and docks; groups follow
  outputs, workspaces diff atomically against what each client was
  last told, and activate, assign and create are backed by real
  mutators
- Protocol: linux-dmabuf speaks v6. feedback carries one sampling tranche
  per card, surface feedback re-leads its tranches when a window crosses
  cards, and an import naming a gpu carrot does not drive fails cleanly
  instead of exploding later
- multi-gpu: every card is brought up and owns its drm device, vulkan
  stack and outputs; a card with nothing plugged in never holds a logical
  device, so it can runtime-suspend. cards carry their sysfs pci address
  for config to name, and `carrot render-probe --all-cards` prints the
  cross-card import matrix
- Present: submitted frames park in a fence-proven yard until the gpu is
  done with them; a cancelled task can neither leak vulkan objects nor
  free what a frame still samples
- Present: frame completion rides a dedicated export semaphore. exporting
  a sync fd from the fence silently reset it, and every later status
  check lied for the rest of the session
- Present: a dmabuf import's first layout transition rides the next
  frame's batch instead of stalling the render thread on queue_wait_idle
  per import, mid-compose
- Present: cached textures age out over present sweeps instead of dying
  every frame a client rotates its buffer set, and they evict the moment
  their buffer, surface or client dies
- Present: frame callbacks drain per output, so a fast head no longer
  fires a slow head's clients early
- Present: the blur cache only bakes on real composes; a capture no
  longer rebuilds the visible output's cache from the wrong scene
- Present: joined modes ride the cursor on an overlay plane, so
  fullscreen dmabuf scans out directly again at >1GHz pixel clocks
- Present: a callback sweep that slips past the next flip coalesces
  into it instead of firing twice; presentation stops reporting
  discarded frames
- Present: replaced dmabufs release after the frames that sampled
  them, not at gpu idle; electron apps no longer hang at startup
- capture: screenshots and casts submit on the io ring and read straight
  out of a pooled persistent mapping. no full-frame copy, no blocking
  wait on the render thread, and a region renders at its own size
  instead of cropping a full-output pass
- capture: serves coalesce their triggers, so a commit storm cannot eat
  the last frame of a burst, and a frame torn down mid-serve never fails
  its successor
- rules: `no-capture` splits video from stills. `no-capture "video"` keeps
  a surface out of screencasts while leaving it in screenshots,
  `no-capture "screenshot"` does the reverse, and a bare `#true` still
  covers both. the split is by protocol: video is the portal's pipewire
  stream (what recorders and clip tools pull through), stills are
  wlr-screencopy and ext-image-copy-capture. a recorder driving
  wlr-screencopy in a loop counts as stills, because nothing on the wire
  says otherwise
- rules: `layer-rule` gained `no-capture`, so a bar or a notification
  overlay can be hidden from recordings without disappearing from
  screenshots. hidden layers become an opaque black stand-in, matching what
  a hidden window already did
- screencast: each cast serves from its own task, one serve in flight,
  later presents coalescing; `max-fps`, `hidden-max-fps`,
  `hidden-refresh`, `default-cursor` and `allow-restore` come from
  config, and a reload lands on running casts
- screencast: a dead cast sweeps immediately, unpins its window and
  emits Session.Closed, so the app stops showing a live-looking share.
  a cast on a hidden workspace keeps feeding, and a captured x window
  stays painted instead of iconifying with its workspace
- screencast: a stale restore token asks for consent again instead of
  casting whatever is focused; a portal session dies with its frontend,
  its request or its connection; a cancelled start cannot leave the
  picker on screen
- resilience: a card's gpu errors climb a shed-then-rebuild ladder, and
  the rebuilt render stack transplants every output's identity so
  clients observe nothing. a flip that never completes on a live device
  trips the same ladder instead of wedging the present loop forever
- resilience: the drm event pump retries reads instead of dying, a
  uevent overflow re-probes and sweeps instead of going deaf, a hotplug
  force-probes only the card the event named, and a fresh input node
  gets a backoff ladder plus a sibling sweep
- resilience: a d-bus call fails on a deadline instead of parking the
  compositor, a lingering old instance loses the bus name instead of
  blocking screen sharing until a reboot, and the pipewire connect is
  bounded so a wedged daemon cannot park the loop thread
- Input: cursor commits wait for an idle screen, so a high-rate mouse
  no longer starves the present loop
- Input: the pointer origin heals under any grab; fullscreen under a
  locked pointer keeps clicks where the cursor is
- Input: SYN_DROPPED reports as a once-a-second rate with a running
  total; the rate is the diagnosis, not the first occurrence
- output: wl_output reports the panel's physical size, refreshed on
  every probe; modes parse and match in millihertz, so
  `2560x1080@100.002` lands on the panel timing it names
- xwayland: surface pairing follows the serial across map cycles, so
  hidden apps come back clickable
- xwayland: fullscreen windows answer configure requests with the
  painted rect instead of the layout tile; game clicks land where the
  cursor is, on every output
- xwayland: hidden workspaces iconify their windows for real
  (WM_STATE), so a buried menu's pointer grab releases the seat
  instead of eating clicks across workspaces
- xwayland: eviction waits for a real client withdraw, so a reviving
  window keeps its layout slot instead of reinserting at a default
  position
- config: a kdl typo reports once, with the offending line and a caret,
  instead of a brace cascade of follow-on errors
- config: a move-workspace-to-output bind action, the first
  workspace-to-output mutator
- render: an ICD that serves no vkCreateInstance fails alone instead of
  panicking the loader; host imports gate once at construction, with
  anv-on-xe off by default until the kernel stops calling a rejected
  bind a device loss
- build: the libc-family build moved out of install into its own module,
  and a dev build heals a missing family from a version-keyed cache.
  registry installs demand an explicit `--target` so crt-static stays
  off host proc-macros, and any recent stable toolchain is enough
- install: a default-prefix install on nixos refuses loudly instead of
  succeeding into a session the display manager cannot see
- session: carrot raises a session target on startup, so units bound to
  `graphical-session.target` can run. xdg-desktop-portal 1.22+ declares
  `Requisite=graphical-session.target`, which fails the activation job
  outright when the target is down, so screencasting was dead with nothing
  but "Dependency failed for Portal service" in the user journal to show
  for it. the compositor's own portal backend was healthy throughout: the
  frontend simply never started. carrot now starts `carrot-session.target`
  (shipped by the package) over d-bus, falling back to nixos's
  `nixos-fake-graphical-session.target`
- nix: the package ships `carrot-session.target` and the module adds it to
  `systemd.packages`

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
