// xdg-shell and wlr-layer-shell.
//
// configures are per-surface, deferred and coalesced by serial. states are
// version gated: TILED_* for new clients, MAXIMIZED as the fallback.

mod layer;
pub mod xdg;
