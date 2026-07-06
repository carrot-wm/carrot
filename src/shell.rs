// xdg-shell and wlr-layer-shell.
//
// configures are per-surface, deferred and coalesced by serial. states are
// version gated: TILED_* for new clients, MAXIMIZED as the fallback.

pub mod layer;
pub mod xdg;

// both shells push onto state.configures; the pump drains them uniformly
pub trait Configurable {
    fn flush_configure(&self);
}
