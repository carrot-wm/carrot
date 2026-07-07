// surface roles. a role is sticky for the surface's lifetime, per spec.
// the role object (surface.ext) is separate from the role enum: ext swaps back
// to NoneExt when the object dies, but the enum never resets.

use super::WlSurface;
use super::commit::PendingState;
use std::rc::Rc;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SurfaceRole {
    None,
    Subsurface,
    Toplevel,
    Popup,
    LayerSurface,
    Xwayland,
    DndIcon,
}

impl SurfaceRole {
    pub fn name(self) -> &'static str {
        match self {
            SurfaceRole::None => "none",
            SurfaceRole::Subsurface => "wl_subsurface",
            SurfaceRole::Toplevel => "xdg_toplevel",
            SurfaceRole::Popup => "xdg_popup",
            SurfaceRole::LayerSurface => "zwlr_layer_surface_v1",
            SurfaceRole::Xwayland => "xwayland_surface_v1",
            SurfaceRole::DndIcon => "dnd_icon",
        }
    }
}

pub trait SurfaceExt {
    /// None consumes the pending state and aborts the commit (sync subsurfaces
    /// stash it into their parent here)
    fn commit_requested(self: Rc<Self>, pending: Box<PendingState>) -> Option<Box<PendingState>> {
        Some(pending)
    }

    fn before_apply(&self) {}

    fn after_apply(&self) {}

    /// Err means a live role object still exists
    fn on_surface_destroy(&self) -> Result<(), ()> {
        Err(())
    }

    /// parent surface, if this role links into a tree
    fn parent(&self) -> Option<Rc<WlSurface>> {
        None
    }

    /// are commits of surfaces below this one parent-synced
    fn effective_sync(&self) -> bool {
        false
    }

    /// keyboard focus landed on / left this surface's window
    fn set_active(&self, _active: bool) {}

    /// the layer surface behind this ext, when the role is LayerSurface
    fn layer_surface(&self) -> Option<std::rc::Rc<crate::shell::layer::LayerSurface>> {
        None
    }
}

pub struct NoneExt;

impl SurfaceExt for NoneExt {
    fn on_surface_destroy(&self) -> Result<(), ()> {
        Ok(())
    }
}
