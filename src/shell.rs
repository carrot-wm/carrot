// xdg-shell and wlr-layer-shell.
//
// configure flow is split AND deferred, with per-surface serials and
// coalescing. states are version gated: the TILED_* set when the client is
// new enough, MAXIMIZED as the fallback for old ones. layer-shell lands
// early - quickshell is half the reason this compositor exists.

mod layer;
mod xdg;
