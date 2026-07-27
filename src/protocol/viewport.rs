// wp_viewporter: per-surface crop and scale.
//
// two independent pieces of double-buffered surface state. the source
// rectangle picks a region of the buffer, the destination size says how big
// the surface is; the source is scaled to fill it. both unset means the
// whole buffer at its natural size, which is what every surface without a
// viewport gets.
//
// the source rectangle is in the coordinates that WOULD be surface-local if
// crop and scale were not applied, i.e. after buffer_transform and
// buffer_scale. that ordering is the whole subtlety of the protocol.


use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{wp_viewport, wp_viewporter};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::surface::WlSurface;
use std::rc::Rc;

const ERR_VIEWPORT_EXISTS: u32 = 0;
const ERR_BAD_VALUE: u32 = 0;
const ERR_BAD_SIZE: u32 = 1;
const ERR_OUT_OF_BOUNDS: u32 = 2;
const ERR_NO_SURFACE: u32 = 3;

pub struct ViewporterGlobal;

impl Global for ViewporterGlobal {
    fn interface(&self) -> &'static str {
        wp_viewporter::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, _version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(Viewporter {
            id,
            client: client.clone(),
        }))
    }
}

pub struct Viewporter {
    pub id: ObjectId,
    pub client: Rc<Client>,
}

impl wp_viewporter::Handler for Viewporter {
    fn destroy(&self, _r: wp_viewporter::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        // destroying the factory leaves live wp_viewport objects alone
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn get_viewport(
        &self,
        req: wp_viewporter::get_viewport::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(surface) = c.objects.surface(req.surface) else {
            c.protocol_error(self.id, ERR_VIEWPORT_EXISTS, "no such surface");
            return Ok(());
        };
        if surface.viewport.borrow().upgrade().is_some() {
            c.protocol_error(
                self.id,
                ERR_VIEWPORT_EXISTS,
                "the surface already has a viewport",
            );
            return Ok(());
        }
        let vp = Rc::new(Viewport {
            id: req.id,
            client: c.clone(),
            surface: Rc::downgrade(&surface),
        });
        c.add_client_obj(vp.clone())?;
        *surface.viewport.borrow_mut() = Rc::downgrade(&vp);
        Ok(())
    }
}

impl Object for Viewporter {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wp_viewporter::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wp_viewporter::dispatch(&*self, 1, opcode, r)
    }
}

pub struct Viewport {
    pub id: ObjectId,
    pub client: Rc<Client>,
    /// weak: the surface may go first, and then every request but destroy
    /// is a no_surface error
    surface: std::rc::Weak<WlSurface>,
}

impl Viewport {
    /// the live surface, or the protocol error the spec demands
    fn surface(&self) -> Option<Rc<WlSurface>> {
        match self.surface.upgrade() {
            Some(s) if !s.destroyed.get() => Some(s),
            _ => {
                self.client
                    .protocol_error(self.id, ERR_NO_SURFACE, "the wl_surface was destroyed");
                None
            }
        }
    }
}

impl wp_viewport::Handler for Viewport {
    fn destroy(&self, _r: wp_viewport::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        // the crop and scale state goes with it, applied at the next commit
        if let Some(s) = self.surface.upgrade() {
            *s.viewport.borrow_mut() = std::rc::Weak::new();
            s.pending_viewport_src.set(Some(None));
            s.pending_viewport_dst.set(Some(None));
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn set_source(&self, r: wp_viewport::set_source::Request) -> Result<(), Box<dyn std::error::Error>> {
        let Some(s) = self.surface() else { return Ok(()) };
        let (x, y, w, h) = (r.x.to_f64(), r.y.to_f64(), r.width.to_f64(), r.height.to_f64());
        // the one magic tuple: all -1 unsets
        if x == -1.0 && y == -1.0 && w == -1.0 && h == -1.0 {
            s.pending_viewport_src.set(Some(None));
            return Ok(());
        }
        if x < 0.0 || y < 0.0 || w <= 0.0 || h <= 0.0 {
            self.client.protocol_error(
                self.id,
                ERR_BAD_VALUE,
                "source rectangle must be positive and non-negative in origin",
            );
            return Ok(());
        }
        s.pending_viewport_src.set(Some(Some((x, y, w, h))));
        Ok(())
    }

    fn set_destination(
        &self,
        r: wp_viewport::set_destination::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(s) = self.surface() else { return Ok(()) };
        if r.width == -1 && r.height == -1 {
            s.pending_viewport_dst.set(Some(None));
            return Ok(());
        }
        if r.width <= 0 || r.height <= 0 {
            self.client.protocol_error(
                self.id,
                ERR_BAD_VALUE,
                "destination size must be positive",
            );
            return Ok(());
        }
        s.pending_viewport_dst.set(Some(Some((r.width, r.height))));
        Ok(())
    }
}

impl Object for Viewport {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wp_viewport::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wp_viewport::dispatch(&*self, 1, opcode, r)
    }
}

/// the errors viewport_size reports, as wire errors on the surface's
/// viewport object. called from apply_state, where the spec says these are
/// raised
pub fn post_apply_error(s: &WlSurface, e: ViewportError) {
    let Some(vp) = s.viewport.borrow().upgrade() else {
        return;
    };
    let (code, msg) = match e {
        ViewportError::BadSize => (ERR_BAD_SIZE, "source size is not integral"),
        ViewportError::OutOfBuffer => (ERR_OUT_OF_BOUNDS, "source rectangle leaves the buffer"),
    };
    vp.client.protocol_error(vp.id, code, msg);
}

/// source rectangle in post-transform, post-scale surface coordinates.
/// wl_fixed carries fractions, so this stays f64 until it meets the texture
pub type Src = (f64, f64, f64, f64);

#[derive(Debug, PartialEq)]
pub enum ViewportError {
    /// src is set, dst is not, and the source size is not integral
    BadSize,
    /// the source rectangle leaves the buffer
    OutOfBuffer,
}

/// the surface size a viewport implies, and the spec's two apply-time
/// errors. `content` is the buffer's size in the same post-transform,
/// post-scale coordinates the source rectangle uses. None means no buffer,
/// which has no size and cannot be out of bounds
pub fn viewport_size(
    src: Option<Src>,
    dst: Option<(i32, i32)>,
    content: Option<(i32, i32)>,
) -> Result<(i32, i32), ViewportError> {
    // a null buffer has no size and cannot be out of bounds, but a
    // destination still describes what the surface would be
    if let Some((sx, sy, sw, sh)) = src
        && let Some((cw, ch)) = content
    {
        // negatives never reach here: set_source rejects them on the wire.
        // the fault is a rectangle that leaves the content area
        if sx + sw > cw as f64 + EPS || sy + sh > ch as f64 + EPS {
            return Err(ViewportError::OutOfBuffer);
        }
    }
    if let Some((w, h)) = dst {
        // the source, whatever its shape, is scaled to exactly this
        return Ok((w, h));
    }
    let Some((sx, sy, sw, sh)) = src else {
        // no crop and no scale: the surface is its content
        return Ok(content.unwrap_or((0, 0)));
    };
    let _ = (sx, sy);
    // cropping without scaling: the size has to land on whole pixels
    if !is_integral(sw) || !is_integral(sh) {
        return Err(ViewportError::BadSize);
    }
    Ok((sw.round() as i32, sh.round() as i32))
}

/// wl_fixed is 1/256, so a value is "integral" when it is within half a
/// fixed step of a whole number. comparing f64 exactly would reject
/// perfectly good client input that survived the fixed-point round trip
const EPS: f64 = 1.0 / 512.0;

fn is_integral(v: f64) -> bool {
    (v - v.round()).abs() < EPS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_viewport_is_the_buffer_size() {
        assert_eq!(viewport_size(None, None, Some((640, 480))), Ok((640, 480)));
        // no buffer, no size
        assert_eq!(viewport_size(None, None, None), Ok((0, 0)));
    }

    #[test]
    fn destination_overrides_the_buffer_size() {
        assert_eq!(
            viewport_size(None, Some((100, 50)), Some((640, 480))),
            Ok((100, 50))
        );
        // and it overrides the source size too: the source is scaled to it
        assert_eq!(
            viewport_size(Some((0.0, 0.0, 320.5, 240.5)), Some((100, 50)), Some((640, 480))),
            Ok((100, 50))
        );
    }

    #[test]
    fn a_crop_without_a_destination_takes_the_source_size() {
        assert_eq!(
            viewport_size(Some((10.0, 20.0, 300.0, 200.0)), None, Some((640, 480))),
            Ok((300, 200))
        );
    }

    #[test]
    fn a_fractional_crop_needs_a_destination() {
        // src set, dst unset, non-integer size: bad_size at apply time
        assert_eq!(
            viewport_size(Some((0.0, 0.0, 300.5, 200.0)), None, Some((640, 480))),
            Err(ViewportError::BadSize)
        );
        // with a destination the fraction is fine, it just gets scaled
        assert_eq!(
            viewport_size(Some((0.0, 0.0, 300.5, 200.0)), Some((64, 64)), Some((640, 480))),
            Ok((64, 64))
        );
    }

    #[test]
    fn a_source_leaving_the_buffer_is_out_of_bounds() {
        assert_eq!(
            viewport_size(Some((400.0, 0.0, 300.0, 200.0)), None, Some((640, 480))),
            Err(ViewportError::OutOfBuffer)
        );
        assert_eq!(
            viewport_size(Some((0.0, 400.0, 300.0, 200.0)), None, Some((640, 480))),
            Err(ViewportError::OutOfBuffer)
        );
        // exactly filling the buffer is in bounds
        assert_eq!(
            viewport_size(Some((0.0, 0.0, 640.0, 480.0)), None, Some((640, 480))),
            Ok((640, 480))
        );
        // a null buffer never raises out_of_buffer
        assert_eq!(
            viewport_size(Some((400.0, 0.0, 300.0, 200.0)), Some((10, 10)), None),
            Ok((10, 10))
        );
    }

    #[test]
    fn a_sized_surface_is_never_smaller_than_one() {
        // the spec floors the surface at 1x1 whenever there is content
        assert_eq!(
            viewport_size(Some((0.0, 0.0, 0.4, 0.4)), None, Some((640, 480))),
            Err(ViewportError::BadSize)
        );
        assert_eq!(
            viewport_size(Some((0.0, 0.0, 1.0, 1.0)), None, Some((640, 480))),
            Ok((1, 1))
        );
    }

    use crate::client::test_utils::test_client;
    use crate::protocol::ObjectId;
    use crate::surface::WlSurface;
    use std::rc::Rc;
    use wp_viewport::Handler as _;
    use wp_viewporter::Handler as _;

    fn setup() -> (Rc<crate::client::Client>, Rc<WlSurface>, Rc<Viewporter>) {
        let (_st, client) = test_client();
        let s = WlSurface::new(ObjectId(700), &client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        let vpr = Rc::new(Viewporter {
            id: ObjectId(701),
            client: client.clone(),
        });
        client.add_client_obj(vpr.clone()).unwrap();
        (client, s, vpr)
    }

    /// (offending object, code) of the first wl_display.error
    fn first_error(bytes: &[u8]) -> Option<(u32, u32)> {
        let mut off = 0;
        while off + 8 <= bytes.len() {
            let obj = u32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap());
            let w2 = u32::from_ne_bytes(bytes[off + 4..off + 8].try_into().unwrap());
            if obj == 1 && w2 & 0xffff == 0 && off + 16 <= bytes.len() {
                return Some((
                    u32::from_ne_bytes(bytes[off + 8..off + 12].try_into().unwrap()),
                    u32::from_ne_bytes(bytes[off + 12..off + 16].try_into().unwrap()),
                ));
            }
            off += ((w2 >> 16) as usize).max(8);
        }
        None
    }

    #[test]
    fn a_surface_gets_only_one_viewport() {
        let (client, _s, vpr) = setup();
        vpr.get_viewport(wp_viewporter::get_viewport::Request {
            id: ObjectId(702),
            surface: ObjectId(700),
        })
        .unwrap();
        assert_eq!(first_error(&client.queued_out_bytes()), None, "first is fine");
        // a second one on the same surface is the documented error
        vpr.get_viewport(wp_viewporter::get_viewport::Request {
            id: ObjectId(703),
            surface: ObjectId(700),
        })
        .unwrap();
        assert_eq!(
            first_error(&client.queued_out_bytes()),
            Some((701, ERR_VIEWPORT_EXISTS))
        );
    }

    #[test]
    fn a_negative_source_is_rejected_on_the_wire() {
        let (client, s, vpr) = setup();
        vpr.get_viewport(wp_viewporter::get_viewport::Request {
            id: ObjectId(702),
            surface: ObjectId(700),
        })
        .unwrap();
        let vp = s.viewport.borrow().upgrade().unwrap();
        vp.set_source(wp_viewport::set_source::Request {
            x: crate::protocol::Fixed::from_f64(-5.0),
            y: crate::protocol::Fixed::from_f64(0.0),
            width: crate::protocol::Fixed::from_f64(10.0),
            height: crate::protocol::Fixed::from_f64(10.0),
        })
        .unwrap();
        assert_eq!(
            first_error(&client.queued_out_bytes()),
            Some((702, ERR_BAD_VALUE))
        );
        // nothing was staged
        assert_eq!(s.pending_viewport_src.get(), None);
    }

    #[test]
    fn the_all_minus_one_tuple_unsets_instead_of_erroring() {
        let (client, s, vpr) = setup();
        vpr.get_viewport(wp_viewporter::get_viewport::Request {
            id: ObjectId(702),
            surface: ObjectId(700),
        })
        .unwrap();
        let vp = s.viewport.borrow().upgrade().unwrap();
        let f = crate::protocol::Fixed::from_f64;
        vp.set_source(wp_viewport::set_source::Request {
            x: f(-1.0),
            y: f(-1.0),
            width: f(-1.0),
            height: f(-1.0),
        })
        .unwrap();
        assert_eq!(s.pending_viewport_src.get(), Some(None), "staged as unset");
        vp.set_destination(wp_viewport::set_destination::Request {
            width: -1,
            height: -1,
        })
        .unwrap();
        assert_eq!(s.pending_viewport_dst.get(), Some(None));
        assert_eq!(first_error(&client.queued_out_bytes()), None, "no error");
    }

    #[test]
    fn a_zero_destination_is_bad_value_but_minus_one_is_not() {
        let (client, s, vpr) = setup();
        vpr.get_viewport(wp_viewporter::get_viewport::Request {
            id: ObjectId(702),
            surface: ObjectId(700),
        })
        .unwrap();
        let vp = s.viewport.borrow().upgrade().unwrap();
        vp.set_destination(wp_viewport::set_destination::Request {
            width: 0,
            height: 10,
        })
        .unwrap();
        assert_eq!(
            first_error(&client.queued_out_bytes()),
            Some((702, ERR_BAD_VALUE))
        );
    }

    #[test]
    fn destroying_the_viewport_stages_the_state_away() {
        let (_client, s, vpr) = setup();
        vpr.get_viewport(wp_viewporter::get_viewport::Request {
            id: ObjectId(702),
            surface: ObjectId(700),
        })
        .unwrap();
        let vp = s.viewport.borrow().upgrade().unwrap();
        vp.set_destination(wp_viewport::set_destination::Request {
            width: 40,
            height: 30,
        })
        .unwrap();
        vp.destroy(wp_viewport::destroy::Request {}).unwrap();
        // the surface forgets it, and the removal rides the next commit
        assert!(s.viewport.borrow().upgrade().is_none());
        assert_eq!(s.pending_viewport_dst.get(), Some(None));
        assert_eq!(s.pending_viewport_src.get(), Some(None));
    }
}
