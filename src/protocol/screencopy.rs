// wlr-screencopy v1: hands output pixels to external capture tools.
// carrot ships no screenshot tool of its own.

use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::rect::Rect;
use std::cell::Cell;
use std::rc::Rc;

const WL_SHM_XRGB8888: u32 = 1;
const WL_SHM_ARGB8888: u32 = 0;
const ERR_INVALID_BUFFER: u32 = 1;

pub struct ScreencopyManagerGlobal;

impl Global for ScreencopyManagerGlobal {
    fn interface(&self) -> &'static str {
        zwlr_screencopy_manager_v1::NAME
    }

    fn version(&self) -> u32 {
        3
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(ScreencopyManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct ScreencopyManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl ScreencopyManager {
    fn make_frame(
        &self,
        id: ObjectId,
        output: ObjectId,
        region: Option<(i32, i32, i32, i32)>,
        overlay_cursor: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let name = c.objects.output(output).map(|o| o.name.clone());
        let geo = name.as_deref().and_then(|n| crate::output::output_geometry(&c.state, n));
        let frame = Rc::new(ScreencopyFrame {
            id,
            client: c.clone(),
            version: self.version,
            output: name,
            rect: Cell::new(Rect::default()),
            used: Cell::new(false),
            overlay_cursor,
        });
        c.add_client_obj(frame.clone())?;
        let Some((_, ow, oh)) = geo else {
            // no display or unknown output: nothing will ever be copyable
            c.event(|o| zwlr_screencopy_frame_v1::failed::send(o, id));
            return Ok(());
        };
        let full = Rect::new_sized_saturating(0, 0, ow as i32, oh as i32);
        let rect = match region {
            Some((x, y, w, h)) => full.intersect(Rect::new_sized_saturating(x, y, w, h)),
            None => full,
        };
        if rect.is_empty() {
            c.event(|o| zwlr_screencopy_frame_v1::failed::send(o, id));
            return Ok(());
        }
        frame.rect.set(rect);
        let (w, h) = (rect.width() as u32, rect.height() as u32);
        let v3 = self.version >= zwlr_screencopy_frame_v1::buffer_done::SINCE;
        c.event(|o| {
            zwlr_screencopy_frame_v1::buffer::send(o, id, WL_SHM_XRGB8888, w, h, w * 4);
            // v3 clients allocate only after the buffer list closes
            if v3 {
                zwlr_screencopy_frame_v1::buffer_done::send(o, id);
            }
        });
        Ok(())
    }
}

impl zwlr_screencopy_manager_v1::Handler for ScreencopyManager {
    fn capture_output(
        &self,
        req: zwlr_screencopy_manager_v1::capture_output::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.make_frame(req.frame, req.output, None, req.overlay_cursor != 0)
    }

    fn capture_output_region(
        &self,
        req: zwlr_screencopy_manager_v1::capture_output_region::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.make_frame(
            req.frame,
            req.output,
            Some((req.x, req.y, req.width, req.height)),
            req.overlay_cursor != 0,
        )
    }

    fn destroy(
        &self,
        _req: zwlr_screencopy_manager_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for ScreencopyManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwlr_screencopy_manager_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwlr_screencopy_manager_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct ScreencopyFrame {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    /// output name at creation; the slot resolves fresh at copy time -
    /// indexes shift under hotplug and a frozen one would land the copy
    /// on whatever output holds it now. None = dead on arrival
    output: Option<String>,
    rect: Cell<Rect>,
    used: Cell<bool>,
    /// compose the pointer into the copy (the wlr overlay_cursor flag)
    overlay_cursor: bool,
}

impl ScreencopyFrame {
    fn do_copy(&self, buffer: ObjectId, with_damage: bool) -> Result<(), Box<dyn std::error::Error>> {
        let req_buffer = buffer;
        let c = &self.client;
        if self.used.replace(true) {
            c.protocol_error(self.id, 0, "the frame was already used");
            return Ok(());
        }
        let Some(name) = &self.output else {
            c.event(|o| zwlr_screencopy_frame_v1::failed::send(o, self.id));
            return Ok(());
        };
        // the output may be gone or replaced since the frame was made;
        // a rect the live output can't satisfy fails, per the protocol,
        // instead of copying another output's pixels
        let Some((slot, ow, oh)) = crate::output::output_geometry(&c.state, name) else {
            c.event(|o| zwlr_screencopy_frame_v1::failed::send(o, self.id));
            return Ok(());
        };
        let Some(buf) = c.objects.buffer(req_buffer) else {
            c.invalid_object(req_buffer);
            return Ok(());
        };
        let rect = self.rect.get();
        if rect.x2 > ow as i32 || rect.y2 > oh as i32 {
            c.event(|o| zwlr_screencopy_frame_v1::failed::send(o, self.id));
            return Ok(());
        }
        let (w, h) = (rect.width() as u32, rect.height() as u32);
        let wl_format = if buf.format.has_alpha() {
            WL_SHM_ARGB8888
        } else {
            WL_SHM_XRGB8888
        };
        let stride = buf.stride as usize;
        if buf.rect.width() as u32 != w
            || buf.rect.height() as u32 != h
            || stride < w as usize * 4
            || (wl_format != WL_SHM_XRGB8888 && wl_format != WL_SHM_ARGB8888)
        {
            c.protocol_error(self.id, ERR_INVALID_BUFFER, "buffer does not fit the frame");
            return Ok(());
        }
        let Some((fd, base)) = buf.shm_write_target() else {
            c.protocol_error(self.id, ERR_INVALID_BUFFER, "copy needs an shm buffer");
            return Ok(());
        };
        let Some(px) = crate::output::screencopy(&c.state, slot, rect, self.overlay_cursor) else {
            c.event(|o| zwlr_screencopy_frame_v1::failed::send(o, self.id));
            return Ok(());
        };
        let row = w as usize * 4;
        let mut ok = true;
        for r in 0..h as usize {
            let off = (base + r * stride) as u64;
            if let Err(e) = rustix::io::pwrite(fd, &px[r * row..][..row], off) {
                eprintln!("carrot: screencopy write failed: {e}");
                ok = false;
                break;
            }
        }
        let id = self.id;
        if !ok {
            c.event(|o| zwlr_screencopy_frame_v1::failed::send(o, id));
            return Ok(());
        }
        let nsec = crate::util::Time::now().nsec();
        let sec = nsec / 1_000_000_000;
        let rem = (nsec % 1_000_000_000) as u32;
        let (dw, dh) = (rect.width() as u32, rect.height() as u32);
        c.event(|o| {
            zwlr_screencopy_frame_v1::flags::send(o, id, 0);
            // no damage tracking yet - report the whole frame dirty (always safe)
            if with_damage {
                zwlr_screencopy_frame_v1::damage::send(o, id, 0, 0, dw, dh);
            }
            zwlr_screencopy_frame_v1::ready::send(
                o,
                id,
                (sec >> 32) as u32,
                sec as u32,
                rem,
            );
        });
        Ok(())
    }
}

impl zwlr_screencopy_frame_v1::Handler for ScreencopyFrame {
    fn copy(
        &self,
        req: zwlr_screencopy_frame_v1::copy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.do_copy(req.buffer, false)
    }

    fn copy_with_damage(
        &self,
        req: zwlr_screencopy_frame_v1::copy_with_damage::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.do_copy(req.buffer, true)
    }

    fn destroy(
        &self,
        _req: zwlr_screencopy_frame_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for ScreencopyFrame {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwlr_screencopy_frame_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwlr_screencopy_frame_v1::dispatch(&*self, self.version, opcode, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use zwlr_screencopy_manager_v1::Handler as _;

    #[test]
    fn headless_frames_fail_cleanly() {
        let (_state, client) = test_client();
        let mgr = ScreencopyManager {
            id: ObjectId(70),
            client: client.clone(),
            version: 1,
        };
        mgr.capture_output(zwlr_screencopy_manager_v1::capture_output::Request {
            frame: ObjectId(71),
            overlay_cursor: 0,
            output: ObjectId(9),
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        // no display: failed, never a buffer advertisement
        assert_eq!(count_events(&bytes, ObjectId(71), 3), 1, "failed");
        assert_eq!(count_events(&bytes, ObjectId(71), 0), 0, "buffer");
    }
}
