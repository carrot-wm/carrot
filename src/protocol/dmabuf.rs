// zwp-linux-dmabuf-v1, version 6. gpu clients hand over dmabufs and the
// renderer samples them in place - no shm round trip. only xrgb/argb is
// advertised; modifiers and their plane counts come from the driver.
//
// v6 exists for multi-gpu: the compositor names a device per tranche and the
// client picks which one to import into. one sampling tranche per card, in
// preference order, and set_sampling_device names which of them a buffer was
// allocated for. a device no card owns can only fail at import, and failing
// at create beats failing at first draw.

use crate::client::{Client, ClientError, Object};
use crate::format::{ARGB8888, Format, XRGB8888};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};
use crate::protocol::shm::{BufferStorage, DmabufImage, DmabufPlane, WlBuffer};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::rect::Rect;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const ERR_ALREADY_USED: u32 = 0;
const ERR_PLANE_IDX: u32 = 1;
const ERR_PLANE_SET: u32 = 2;
const ERR_INCOMPLETE: u32 = 3;
const ERR_INVALID_FORMAT: u32 = 4;
const ERR_INVALID_DIMENSIONS: u32 = 5;
const ERR_OUT_OF_BOUNDS: u32 = 6;
const ERR_INVALID_WL_BUFFER: u32 = 7;
const ERR_INVALID_DEV_T_SIZE: u32 = 8;

/// tranche_flags bit for "the compositor can sample from this device". v6
/// demands at least one flag per tranche and at least one sampling tranche;
/// the scanout bit (1) stays unset because the advertised set comes from
/// sample_modifiers, and the display engine takes a narrower list
const TRANCHE_SAMPLING: u32 = 2;

const MOD_LINEAR: u64 = 0;
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const MAX_PLANES: usize = 4;

/// one gpu's advertised set
pub struct CardFormats {
    /// primary node dev_t; clients resolve it to the matching render node,
    /// and name it back through set_sampling_device
    pub devnum: u64,
    /// (fourcc, modifier, plane count) triples
    pub formats: Vec<(u32, u64, u32)>,
}

/// what display bring-up found on every render device; feeds the feedback
/// tranches and the modifier advertisement
pub struct DmabufInfo {
    /// preference order, index 0 is the primary
    pub cards: Vec<CardFormats>,
}

impl DmabufInfo {
    /// plane count for a format+modifier pair on any card that advertises
    /// it. a client may allocate for whichever gpu it was steered to, so a
    /// pair is legal if any of them takes it
    fn plane_count(&self, fourcc: u32, modifier: u64) -> Option<usize> {
        self.cards
            .iter()
            .flat_map(|c| c.formats.iter())
            .find(|&&(f, m, _)| f == fourcc && m == modifier)
            .map(|&(_, _, n)| n as usize)
    }

    fn has_device(&self, devnum: u64) -> bool {
        self.cards.iter().any(|c| c.devnum == devnum)
    }
}

/// which cards to emit tranches for, in order. `prefer` is the devnum of
/// the card that composites the surface this feedback belongs to; it leads,
/// and everything else keeps primary order behind it
fn tranche_order(info: &DmabufInfo, prefer: Option<u64>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..info.cards.len()).collect();
    if let Some(want) = prefer
        && let Some(pos) = info.cards.iter().position(|c| c.devnum == want)
    {
        order.remove(pos);
        order.insert(0, pos);
    }
    order
}

fn linear_fallback() -> Vec<(u32, u64, u32)> {
    vec![(XRGB8888.drm, MOD_LINEAR, 1), (ARGB8888.drm, MOD_LINEAR, 1)]
}

fn fourcc(format: u32) -> Option<&'static Format> {
    if format == XRGB8888.drm {
        Some(&XRGB8888)
    } else if format == ARGB8888.drm {
        Some(&ARGB8888)
    } else {
        None
    }
}

pub struct DmabufGlobal;

impl Global for DmabufGlobal {
    fn interface(&self) -> &'static str {
        zwp_linux_dmabuf_v1::NAME
    }

    fn version(&self) -> u32 {
        6
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(Dmabuf {
            id,
            client: client.clone(),
            version,
        }))?;
        // v4 clients get everything through feedback objects
        if version >= 4 {
            return Ok(());
        }
        let info = client.state.dmabuf_info.borrow();
        // pre-v4 has no way to name a device, so it gets the primary's set
        let formats = match info.as_ref().and_then(|i| i.cards.first()) {
            Some(c) => c.formats.clone(),
            None => linear_fallback(),
        };
        drop(info);
        client.event(|o| {
            for &(fourcc, modifier, _) in &formats {
                if version >= 3 {
                    zwp_linux_dmabuf_v1::modifier::send(
                        o,
                        id,
                        fourcc,
                        (modifier >> 32) as u32,
                        modifier as u32,
                    );
                } else {
                    zwp_linux_dmabuf_v1::format::send(o, id, fourcc);
                }
            }
        });
        Ok(())
    }
}

pub struct Dmabuf {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl zwp_linux_dmabuf_v1::Handler for Dmabuf {
    fn destroy(
        &self,
        _req: zwp_linux_dmabuf_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn create_params(
        &self,
        req: zwp_linux_dmabuf_v1::create_params::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.add_client_obj(Rc::new(BufferParams {
            id: req.params_id,
            client: self.client.clone(),
            version: self.version,
            planes: RefCell::new(Vec::new()),
            modifier: Cell::new(None),
            used: Cell::new(false),
            sampling_device: Cell::new(None),
        }))?;
        Ok(())
    }

    fn get_default_feedback(
        &self,
        req: zwp_linux_dmabuf_v1::get_default_feedback::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // no surface to follow, so the primary leads
        feedback(&self.client, req.id, self.version, None)
    }

    fn get_surface_feedback(
        &self,
        req: zwp_linux_dmabuf_v1::get_surface_feedback::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // the card composing this surface's output leads its tranches, so a
        // client that follows the hint allocates where we sample
        let surface = self.client.objects.surface(req.surface);
        feedback(&self.client, req.id, self.version, surface)
    }
}

/// feedback is static per card set, so send the whole state up front and be
/// done. one tranche per card, all flagged sampling, in `prefer` order
fn feedback(
    c: &Rc<Client>,
    id: ObjectId,
    version: u32,
    surface: Option<Rc<crate::surface::WlSurface>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let fb = Rc::new(Feedback {
        id,
        client: c.clone(),
        version,
        last_order: RefCell::new(Vec::new()),
        surface: surface.as_ref().map(Rc::downgrade),
    });
    c.add_client_obj(fb.clone())?;
    let prefer = surface.and_then(|s| card_of_surface(&c.state, &s));
    if fb.surface.is_some() {
        c.state.dmabuf_feedbacks.borrow_mut().push(Rc::downgrade(&fb));
    }
    send_feedback(&fb, prefer)
}

/// the parameter set itself, re-sendable when the surface's card changes
fn send_feedback(fb: &Feedback, prefer: Option<u64>) -> Result<(), Box<dyn std::error::Error>> {
    let (c, id, version) = (&fb.client, fb.id, fb.version);
    let guard = c.state.dmabuf_info.borrow();
    // no probe yet: one nameless linear tranche, which is what a client can
    // always fall back to
    let fallback = DmabufInfo {
        cards: vec![CardFormats {
            devnum: 0,
            formats: linear_fallback(),
        }],
    };
    let (info, named) = match guard.as_ref() {
        Some(i) if !i.cards.is_empty() => (i, true),
        _ => (&fallback, false),
    };
    let order = tranche_order(info, prefer);
    // identical parameters twice in a row are exactly what the spec asks
    // compositors to avoid; a surface moving within one card changes nothing
    let devnums: Vec<u64> = order.iter().map(|&i| info.cards[i].devnum).collect();
    if *fb.last_order.borrow() == devnums {
        return Ok(());
    }
    *fb.last_order.borrow_mut() = devnums;

    // one table for every card; each tranche indexes its own slice of it
    let mut table = Vec::new();
    let mut spans: Vec<(u16, u16)> = Vec::new();
    for card in &info.cards {
        let first = (table.len() / 16) as u16;
        for &(fourcc, modifier, _) in &card.formats {
            table.extend_from_slice(&fourcc.to_ne_bytes());
            table.extend_from_slice(&0u32.to_ne_bytes());
            table.extend_from_slice(&modifier.to_ne_bytes());
        }
        spans.push((first, card.formats.len() as u16));
    }
    let fd = rustix::fs::memfd_create("carrot-dmabuf-table", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| format!("memfd: {e}"))?;
    {
        use std::io::Write as _;
        let mut f = std::fs::File::from(fd.try_clone().map_err(|e| format!("dup: {e}"))?);
        f.write_all(&table).map_err(|e| format!("table write: {e}"))?;
    }
    let fd = Rc::new(fd);
    let table_len = table.len() as u32;
    // v6 retired main_device; the sampling tranches carry the devices. an
    // older client still needs one, and it names the primary
    let main = (version < 6 && named).then(|| info.cards[0].devnum.to_ne_bytes());
    let tranches: Vec<(Option<[u8; 8]>, Vec<u8>)> = order
        .iter()
        .map(|&i| {
            let (first, n) = spans[i];
            let idx: Vec<u8> = (first..first + n).flat_map(|x| x.to_ne_bytes()).collect();
            (named.then(|| info.cards[i].devnum.to_ne_bytes()), idx)
        })
        .collect();
    // everything the emit needs is copied out; release the borrow before
    // handing control to the event writer
    drop(guard);

    c.event(|o| {
        zwp_linux_dmabuf_feedback_v1::format_table::send(o, id, fd.clone(), table_len);
        if let Some(dev) = main {
            zwp_linux_dmabuf_feedback_v1::main_device::send(o, id, &dev);
        }
        for (dev, idx) in &tranches {
            if let Some(dev) = dev {
                zwp_linux_dmabuf_feedback_v1::tranche_target_device::send(o, id, dev);
            }
            // v6 demands at least one flag per tranche and at least one
            // sampling tranche; every card carrot renders on is one
            let flags = if version >= 6 { TRANCHE_SAMPLING } else { 0 };
            zwp_linux_dmabuf_feedback_v1::tranche_flags::send(o, id, flags);
            zwp_linux_dmabuf_feedback_v1::tranche_formats::send(o, id, idx);
            zwp_linux_dmabuf_feedback_v1::tranche_done::send(o, id);
        }
        zwp_linux_dmabuf_feedback_v1::done::send(o, id);
    });
    Ok(())
}


pub struct Feedback {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    /// devnums in the order last sent. the spec asks compositors not to
    /// send the exact same parameters twice in a row, so a surface moving
    /// between outputs on one card re-sends nothing
    last_order: RefCell<Vec<u64>>,
    /// the surface this feedback speaks for; None for the default object,
    /// which has no output to follow
    surface: Option<std::rc::Weak<crate::surface::WlSurface>>,
}

impl Feedback {
    /// re-send unless the card order is unchanged; send_feedback holds the
    /// "not the same parameters twice" rule
    fn resend(&self, prefer: Option<u64>) {
        if let Err(e) = send_feedback(self, prefer) {
            crate::trace!("dmabuf: feedback resend failed: {e}");
        }
    }
}

/// the card composing the output this window currently sits on
fn card_of_window(
    state: &Rc<crate::state::State>,
    win: &Rc<crate::tree::Window>,
) -> Option<u64> {
    let slot = crate::tree::workspace_of(state, win)?.output.get();
    let d = state.display.borrow();
    let out = d.as_ref()?.outputs.borrow().get(slot)?.clone();
    Some(out.render_devnum())
}

fn card_of_surface(
    state: &Rc<crate::state::State>,
    s: &Rc<crate::surface::WlSurface>,
) -> Option<u64> {
    let win = crate::tree::window_for_surface_any(state, s)?;
    card_of_window(state, &win)
}

/// a window crossed to an output on a different card: its surfaces should
/// start allocating there. dead entries are swept on the way through
pub fn output_changed(state: &Rc<crate::state::State>, win: &Rc<crate::tree::Window>) {
    let live: Vec<Rc<Feedback>> = {
        let mut reg = state.dmabuf_feedbacks.borrow_mut();
        reg.retain(|w| w.strong_count() > 0);
        reg.iter().filter_map(|w| w.upgrade()).collect()
    };
    if live.is_empty() {
        return;
    }
    // the caller already has the window, so resolve the card from it
    // rather than walking every workspace back from the surface
    let Some(dev) = card_of_window(state, win) else {
        return;
    };
    // the toplevel's own surface: that is what a client asks surface
    // feedback for. a subsurface keeps whatever order it was created with
    let root = win.surface();
    for fb in live {
        let Some(fs) = fb.surface.as_ref().and_then(|w| w.upgrade()) else {
            continue;
        };
        if Rc::ptr_eq(&root, &fs) {
            fb.resend(Some(dev));
        }
    }
}

impl zwp_linux_dmabuf_feedback_v1::Handler for Feedback {
    fn destroy(
        &self,
        _req: zwp_linux_dmabuf_feedback_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for Feedback {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_linux_dmabuf_feedback_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_linux_dmabuf_feedback_v1::dispatch(&*self, self.version, opcode, r)
    }
}

impl Object for Dmabuf {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_linux_dmabuf_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_linux_dmabuf_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct BufferParams {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    planes: RefCell<Vec<DmabufPlane>>,
    /// all planes must agree on it
    modifier: Cell<Option<u64>>,
    used: Cell<bool>,
    /// v6: which gpu the client wants this imported into. unset means "you
    /// pick", which for one card is the same answer
    sampling_device: Cell<Option<u64>>,
}

impl BufferParams {
    /// shared tail of create/create_immed; None means an error was posted
    fn build(
        &self,
        width: i32,
        height: i32,
        format: u32,
    ) -> Option<(&'static Format, DmabufImage)> {
        let c = &self.client;
        if self.used.replace(true) {
            c.protocol_error(self.id, ERR_ALREADY_USED, "params were already used");
            return None;
        }
        let planes = std::mem::take(&mut *self.planes.borrow_mut());
        if planes.is_empty() {
            c.protocol_error(self.id, ERR_INCOMPLETE, "no planes were added");
            return None;
        }
        let modifier = self.modifier.get().unwrap_or(MOD_INVALID);
        let Some(format) = fourcc(format) else {
            c.protocol_error(
                self.id,
                ERR_INVALID_FORMAT,
                &format!("format {format:#x} is not advertised"),
            );
            return None;
        };
        if width <= 0 || height <= 0 || width > 16384 || height > 16384 {
            c.protocol_error(self.id, ERR_INVALID_DIMENSIONS, "bad buffer dimensions");
            return None;
        }
        // the pair must be one we advertised; implicit falls back to linear
        let modifier = if modifier == MOD_INVALID { MOD_LINEAR } else { modifier };
        let expected = match c.state.dmabuf_info.borrow().as_ref() {
            Some(i) => i.plane_count(format.drm, modifier),
            None => (modifier == MOD_LINEAR).then_some(1),
        };
        let Some(expected) = expected else {
            c.protocol_error(
                self.id,
                ERR_INVALID_FORMAT,
                &format!("modifier {modifier:#x} is not advertised for this format"),
            );
            return None;
        };
        if planes.len() != expected {
            c.protocol_error(self.id, ERR_INCOMPLETE, "plane count does not match the modifier");
            return None;
        }
        // only linear layouts are transparent enough to bounds-check; the
        // rest are driver-opaque and get validated at import
        if modifier == MOD_LINEAR {
            let plane = &planes[0];
            let size = rustix::fs::seek(&plane.fd, rustix::fs::SeekFrom::End(0)).unwrap_or(0);
            let need = plane.offset as u64
                + plane.stride as u64 * (height as u64 - 1)
                + width as u64 * 4;
            if plane.stride < width as u32 * 4 || need > size {
                c.protocol_error(self.id, ERR_OUT_OF_BOUNDS, "planes exceed the dmabuf");
                return None;
            }
        }
        Some((format, DmabufImage { planes, modifier }))
    }

    /// the client named a gpu carrot does not drive. the spec makes this a
    /// failed import rather than a protocol error, because the device list
    /// can change under the client between feedback and create
    fn foreign_sampling_device(&self) -> bool {
        let Some(want) = self.sampling_device.get() else {
            return false;
        };
        match self.client.state.dmabuf_info.borrow().as_ref() {
            Some(i) => !i.has_device(want),
            // nothing probed yet, so there is no device to contradict
            None => false,
        }
    }

    /// one bo backs the whole image: the import reads memory only from
    /// plane 0, so disjoint buffers can never bind correctly
    fn single_bo(img: &DmabufImage) -> bool {
        let Some((first, rest)) = img.planes.split_first() else {
            return true;
        };
        let Ok(base) = rustix::fs::fstat(&first.fd) else {
            return false;
        };
        rest.iter().all(|p| {
            rustix::fs::fstat(&p.fd)
                .is_ok_and(|st| (st.st_dev, st.st_ino) == (base.st_dev, base.st_ino))
        })
    }

    fn buffer(
        &self,
        id: ObjectId,
        w: i32,
        h: i32,
        format: &'static Format,
        img: DmabufImage,
    ) -> Rc<WlBuffer> {
        Rc::new(WlBuffer {
            id,
            uid: self.client.state.next_uid(),
            client: self.client.clone(),
            rect: Rect::new_sized_saturating(0, 0, w, h),
            format,
            stride: img.planes[0].stride as i32,
            storage: BufferStorage::Dmabuf(img),
            destroyed: Cell::new(false),
        })
    }
}

impl zwp_linux_buffer_params_v1::Handler for BufferParams {
    fn destroy(
        &self,
        _req: zwp_linux_buffer_params_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn add(
        &self,
        req: zwp_linux_buffer_params_v1::add::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        if self.used.get() {
            c.protocol_error(self.id, ERR_ALREADY_USED, "params were already used");
            return Ok(());
        }
        let mut planes = self.planes.borrow_mut();
        if req.plane_idx as usize != planes.len() || planes.len() >= MAX_PLANES {
            c.protocol_error(
                self.id,
                if planes.len() >= MAX_PLANES { ERR_PLANE_IDX } else { ERR_PLANE_SET },
                &format!("plane {} out of order or over the limit", req.plane_idx),
            );
            return Ok(());
        }
        let modifier = ((req.modifier_hi as u64) << 32) | req.modifier_lo as u64;
        if let Some(prev) = self.modifier.get() {
            if prev != modifier {
                // v5 pinned this to invalid_format; it used to be unspecified
                c.protocol_error(self.id, ERR_INVALID_FORMAT, "planes disagree on the modifier");
                return Ok(());
            }
        } else {
            self.modifier.set(Some(modifier));
        }
        planes.push(DmabufPlane {
            fd: req.fd,
            offset: req.offset,
            stride: req.stride,
        });
        Ok(())
    }

    fn create(
        &self,
        req: zwp_linux_buffer_params_v1::create::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some((format, img)) = self.build(req.width, req.height, req.format) else {
            return Ok(());
        };
        if req.flags != 0 || !Self::single_bo(&img) || self.foreign_sampling_device() {
            // import failures on the async path answer with failed, not
            // a protocol violation; the client falls back
            c.event(|o| zwp_linux_buffer_params_v1::failed::send(o, self.id));
            return Ok(());
        }
        let id = c.objects.alloc_server_id();
        let buf = self.buffer(id, req.width, req.height, format, img);
        c.add_server_obj(buf.clone());
        c.objects.track_buffer(buf);
        c.event(|o| zwp_linux_buffer_params_v1::created::send(o, self.id, id));
        Ok(())
    }

    fn create_immed(
        &self,
        req: zwp_linux_buffer_params_v1::create_immed::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some((format, img)) = self.build(req.width, req.height, req.format) else {
            return Ok(());
        };
        if req.flags != 0 {
            c.protocol_error(self.id, ERR_INVALID_WL_BUFFER, "buffer flags are unsupported");
            return Ok(());
        }
        if !Self::single_bo(&img) {
            c.protocol_error(self.id, ERR_INVALID_WL_BUFFER, "planes span multiple buffers");
            return Ok(());
        }
        if self.foreign_sampling_device() {
            // create_immed has no failed event, so a doomed import is fatal
            c.protocol_error(self.id, ERR_INVALID_WL_BUFFER, "sampling device is not this gpu");
            return Ok(());
        }
        let buf = self.buffer(req.buffer_id, req.width, req.height, format, img);
        c.add_client_obj(buf.clone())?;
        c.objects.track_buffer(buf);
        Ok(())
    }

    fn set_sampling_device(
        &self,
        req: zwp_linux_buffer_params_v1::set_sampling_device::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Ok(dev) = <[u8; 8]>::try_from(&req.device[..]) else {
            c.protocol_error(
                self.id,
                ERR_INVALID_DEV_T_SIZE,
                &format!("device array is {} bytes, not a dev_t", req.device.len()),
            );
            return Ok(());
        };
        self.sampling_device.set(Some(u64::from_ne_bytes(dev)));
        Ok(())
    }
}

impl Object for BufferParams {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_linux_buffer_params_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_linux_buffer_params_v1::dispatch(&*self, self.version, opcode, r)
    }
}

// wl_buffer's own dispatch lives in shm.rs and is storage-agnostic; nothing
// dmabuf-specific to add here. keep the import lazy: the renderer wraps the
// fd the first time the buffer is actually drawn.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use rustix::fs::{MemfdFlags, memfd_create};
    use std::io::Write as _;
    use std::os::fd::OwnedFd;
    use zwp_linux_buffer_params_v1::Handler as _;
    use zwp_linux_dmabuf_v1::Handler as _;

    fn fake_dmabuf(bytes: usize) -> OwnedFd {
        let fd = memfd_create("fake-dmabuf", MemfdFlags::CLOEXEC).unwrap();
        let mut f = std::fs::File::from(fd);
        f.write_all(&vec![0u8; bytes]).unwrap();
        f.into()
    }

    fn one_card(devnum: u64, formats: Vec<(u32, u64, u32)>) -> DmabufInfo {
        DmabufInfo {
            cards: vec![CardFormats { devnum, formats }],
        }
    }

    fn bare_params(client: &Rc<Client>, id: u32) -> Rc<BufferParams> {
        Rc::new(BufferParams {
            id: ObjectId(id),
            client: client.clone(),
            version: 6,
            planes: RefCell::new(Vec::new()),
            modifier: Cell::new(None),
            used: Cell::new(false),
            sampling_device: Cell::new(None),
        })
    }

    fn params(client: &Rc<Client>) -> Rc<BufferParams> {
        let mgr = Dmabuf {
            id: ObjectId(80),
            client: client.clone(),
            version: 3,
        };
        mgr.create_params(zwp_linux_dmabuf_v1::create_params::Request {
            params_id: ObjectId(81),
        })
        .unwrap();
        bare_params(client, 81)
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

    /// every occurrence's array argument, in send order
    fn event_arrays(bytes: &[u8], object: ObjectId, opcode: u32) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut off = 0;
        while off + 8 <= bytes.len() {
            let obj = u32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap());
            let w2 = u32::from_ne_bytes(bytes[off + 4..off + 8].try_into().unwrap());
            let len = ((w2 >> 16) as usize).max(8);
            if obj == object.0 && w2 & 0xffff == opcode && off + 12 <= bytes.len() {
                let n = u32::from_ne_bytes(bytes[off + 8..off + 12].try_into().unwrap()) as usize;
                if off + 12 + n <= bytes.len() {
                    out.push(bytes[off + 12..off + 12 + n].to_vec());
                }
            }
            off += len;
        }
        out
    }

    /// first u32 argument of an event, for checking tranche flags
    fn event_arg(bytes: &[u8], object: ObjectId, opcode: u32) -> Option<u32> {
        let mut off = 0;
        while off + 8 <= bytes.len() {
            let obj = u32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap());
            let w2 = u32::from_ne_bytes(bytes[off + 4..off + 8].try_into().unwrap());
            if obj == object.0 && w2 & 0xffff == opcode && off + 12 <= bytes.len() {
                return Some(u32::from_ne_bytes(bytes[off + 8..off + 12].try_into().unwrap()));
            }
            off += ((w2 >> 16) as usize).max(8);
        }
        None
    }

    #[test]
    fn create_immed_builds_a_dmabuf_buffer() {
        let (_state, client) = test_client();
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(64 * 64 * 4),
            plane_idx: 0,
            offset: 0,
            stride: 64 * 4,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        let buf = client.objects.buffer(ObjectId(82)).unwrap();
        assert!(buf.dmabuf().is_some());
        assert!(buf.shm_access().is_none());
        assert_eq!(buf.rect.width(), 64);
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 0);
    }

    #[test]
    fn an_undersized_dmabuf_is_rejected() {
        let (_state, client) = test_client();
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(1024),
            plane_idx: 0,
            offset: 0,
            stride: 64 * 4,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }

    #[test]
    fn feedback_sends_the_whole_state() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(one_card(0xe280, vec![
                (XRGB8888.drm, 0, 1),
                (XRGB8888.drm, 42, 1),
                (ARGB8888.drm, 42, 1),
            ]));
        feedback(&client, ObjectId(90), 4, None).unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, ObjectId(90), 1), 1, "format_table");
        assert_eq!(count_events(&bytes, ObjectId(90), 2), 1, "main_device");
        // pre-v6 tranches carry no flags
        assert_eq!(event_arg(&bytes, ObjectId(90), 6), Some(0), "tranche_flags");
        assert_eq!(count_events(&bytes, ObjectId(90), 4), 1, "tranche_target_device");
        assert_eq!(count_events(&bytes, ObjectId(90), 5), 1, "tranche_formats");
        assert_eq!(count_events(&bytes, ObjectId(90), 3), 1, "tranche_done");
        assert_eq!(count_events(&bytes, ObjectId(90), 0), 1, "done");
    }

    #[test]
    fn modifiers_gate_on_the_advertised_set() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(one_card(0, vec![(XRGB8888.drm, 42, 1)]));
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(4096),
            plane_idx: 0,
            offset: 0,
            stride: 64,
            modifier_hi: 0,
            modifier_lo: 42,
        })
        .unwrap();
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 8,
            height: 8,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        // tiled path skips the linear bounds check and lands
        let buf = client.objects.buffer(ObjectId(82)).unwrap();
        assert_eq!(buf.dmabuf().unwrap().modifier, 42);
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 0);

        // an unadvertised modifier is a loud error
        let p2 = bare_params(&client, 84);
        p2.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(4096),
            plane_idx: 0,
            offset: 0,
            stride: 64,
            modifier_hi: 0,
            modifier_lo: 7,
        })
        .unwrap();
        p2.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(83),
            width: 8,
            height: 8,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }

    #[test]
    fn create_with_unsupported_flags_sends_failed() {
        let (_state, client) = test_client();
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(64 * 64 * 4),
            plane_idx: 0,
            offset: 0,
            stride: 64 * 4,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        p.create(zwp_linux_buffer_params_v1::create::Request {
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 1,
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        // the client survives and gets failed, not created
        assert_eq!(count_events(&bytes, ObjectId(1), 0), 0);
        assert_eq!(count_events(&bytes, ObjectId(81), 1), 1, "failed");
        assert_eq!(count_events(&bytes, ObjectId(81), 0), 0, "created");
    }

    #[test]
    fn create_immed_with_unsupported_flags_is_fatal() {
        let (_state, client) = test_client();
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(64 * 64 * 4),
            plane_idx: 0,
            offset: 0,
            stride: 64 * 4,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 1,
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }

    #[test]
    fn disjoint_plane_buffers_fail_without_killing_the_async_client() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(one_card(0, vec![(XRGB8888.drm, 42, 2)]));
        let p = params(&client);
        // two planes, two unrelated buffers: the import reads only one
        for idx in 0..2u32 {
            p.add(zwp_linux_buffer_params_v1::add::Request {
                fd: fake_dmabuf(4096),
                plane_idx: idx,
                offset: 0,
                stride: 64,
                modifier_hi: 0,
                modifier_lo: 42,
            })
            .unwrap();
        }
        p.create(zwp_linux_buffer_params_v1::create::Request {
            width: 8,
            height: 8,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, ObjectId(1), 0), 0, "client survives");
        assert_eq!(count_events(&bytes, ObjectId(81), 1), 1, "failed");
        assert_eq!(count_events(&bytes, ObjectId(81), 0), 0, "created");
    }

    #[test]
    fn plane_count_must_match_the_modifier() {
        let (_state, client) = test_client();
        let p = params(&client);
        for idx in 0..2u32 {
            p.add(zwp_linux_buffer_params_v1::add::Request {
                fd: fake_dmabuf(64 * 64 * 4),
                plane_idx: idx,
                offset: 0,
                stride: 64 * 4,
                modifier_hi: 0,
                modifier_lo: 0,
            })
            .unwrap();
        }
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        // linear is single-plane; two planes die before touching the driver
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }

    #[test]
    fn multi_plane_modifier_accepts_only_the_driver_count() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(one_card(0, vec![(XRGB8888.drm, 42, 2)]));
        let p = params(&client);
        // both planes ride the same bo, like a real tiled allocation
        let bo = fake_dmabuf(4096);
        for idx in 0..2u32 {
            p.add(zwp_linux_buffer_params_v1::add::Request {
                fd: bo.try_clone().unwrap(),
                plane_idx: idx,
                offset: 0,
                stride: 64,
                modifier_hi: 0,
                modifier_lo: 42,
            })
            .unwrap();
        }
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 8,
            height: 8,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        // the driver-reported count lands as-is
        let buf = client.objects.buffer(ObjectId(82)).unwrap();
        assert_eq!(buf.dmabuf().unwrap().planes.len(), 2);
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 0);

        // a single plane under the same modifier is short
        let p2 = bare_params(&client, 84);
        p2.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(4096),
            plane_idx: 0,
            offset: 0,
            stride: 64,
            modifier_hi: 0,
            modifier_lo: 42,
        })
        .unwrap();
        p2.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(83),
            width: 8,
            height: 8,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }

    #[test]
    fn v6_feedback_drops_main_device_and_flags_the_tranche() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(one_card(0xe280, vec![(XRGB8888.drm, 0, 1)]));
        feedback(&client, ObjectId(90), 6, None).unwrap();
        let bytes = client.queued_out_bytes();
        // v6 retired main_device entirely
        assert_eq!(count_events(&bytes, ObjectId(90), 2), 0, "main_device");
        // and demands a flag on every tranche, with sampling somewhere
        assert_eq!(count_events(&bytes, ObjectId(90), 4), 1, "tranche_target_device");
        assert_eq!(
            event_arg(&bytes, ObjectId(90), 6),
            Some(TRANCHE_SAMPLING),
            "tranche_flags"
        );
        assert_eq!(count_events(&bytes, ObjectId(90), 3), 1, "tranche_done");
        assert_eq!(count_events(&bytes, ObjectId(90), 0), 1, "done");
    }

    #[test]
    fn a_short_sampling_device_is_a_dev_t_error() {
        let (_state, client) = test_client();
        let p = params(&client);
        p.set_sampling_device(zwp_linux_buffer_params_v1::set_sampling_device::Request {
            device: vec![0u8; 4],
        })
        .unwrap();
        assert_eq!(
            first_error(&client.queued_out_bytes()),
            Some((81, ERR_INVALID_DEV_T_SIZE))
        );
    }

    #[test]
    fn the_advertised_sampling_device_imports() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(one_card(0xe280, vec![(XRGB8888.drm, MOD_LINEAR, 1)]));
        let p = params(&client);
        p.set_sampling_device(zwp_linux_buffer_params_v1::set_sampling_device::Request {
            device: 0xe280u64.to_ne_bytes().to_vec(),
        })
        .unwrap();
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(64 * 64 * 4),
            plane_idx: 0,
            offset: 0,
            stride: 64 * 4,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        assert!(client.objects.buffer(ObjectId(82)).is_some());
        assert_eq!(first_error(&client.queued_out_bytes()), None);
    }

    #[test]
    fn a_foreign_sampling_device_fails_instead_of_importing() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(one_card(0xe280, vec![(XRGB8888.drm, MOD_LINEAR, 1)]));
        let p = params(&client);
        // a second gpu carrot does not drive
        p.set_sampling_device(zwp_linux_buffer_params_v1::set_sampling_device::Request {
            device: 0xe2c0u64.to_ne_bytes().to_vec(),
        })
        .unwrap();
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(64 * 64 * 4),
            plane_idx: 0,
            offset: 0,
            stride: 64 * 4,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        p.create(zwp_linux_buffer_params_v1::create::Request {
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        // the async path stays recoverable: failed, and the client lives
        assert_eq!(first_error(&bytes), None, "client survives");
        assert_eq!(count_events(&bytes, ObjectId(81), 1), 1, "failed");
        assert_eq!(count_events(&bytes, ObjectId(81), 0), 0, "created");
    }

    #[test]
    fn a_foreign_sampling_device_is_fatal_for_create_immed() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(one_card(0xe280, vec![(XRGB8888.drm, MOD_LINEAR, 1)]));
        let p = params(&client);
        p.set_sampling_device(zwp_linux_buffer_params_v1::set_sampling_device::Request {
            device: 0xe2c0u64.to_ne_bytes().to_vec(),
        })
        .unwrap();
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(64 * 64 * 4),
            plane_idx: 0,
            offset: 0,
            stride: 64 * 4,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        p.create_immed(zwp_linux_buffer_params_v1::create_immed::Request {
            buffer_id: ObjectId(82),
            width: 64,
            height: 64,
            format: XRGB8888.drm,
            flags: 0,
        })
        .unwrap();
        // create_immed has no failed event, so the doomed import kills it
        assert_eq!(
            first_error(&client.queued_out_bytes()),
            Some((81, ERR_INVALID_WL_BUFFER))
        );
        assert!(client.objects.buffer(ObjectId(82)).is_none());
    }

    #[test]
    fn planes_disagreeing_on_the_modifier_is_invalid_format() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(one_card(0, vec![(XRGB8888.drm, 42, 2)]));
        let p = params(&client);
        for (idx, modifier_lo) in [(0u32, 42u32), (1, 43)] {
            p.add(zwp_linux_buffer_params_v1::add::Request {
                fd: fake_dmabuf(4096),
                plane_idx: idx,
                offset: 0,
                stride: 64,
                modifier_hi: 0,
                modifier_lo,
            })
            .unwrap();
        }
        // v5 pinned this to invalid_format, not invalid_wl_buffer
        assert_eq!(
            first_error(&client.queued_out_bytes()),
            Some((81, ERR_INVALID_FORMAT))
        );
    }

    #[test]
    fn params_are_single_use_and_single_plane() {
        let (_state, client) = test_client();
        let p = params(&client);
        p.add(zwp_linux_buffer_params_v1::add::Request {
            fd: fake_dmabuf(4096),
            plane_idx: 1,
            offset: 0,
            stride: 64,
            modifier_hi: 0,
            modifier_lo: 0,
        })
        .unwrap();
        // plane_idx 1 is a protocol error straight away
        assert_eq!(count_events(&client.queued_out_bytes(), ObjectId(1), 0), 1);
    }

    #[test]
    fn the_surfaces_card_leads_the_tranches() {
        let info = DmabufInfo {
            cards: vec![
                CardFormats { devnum: 0xe200, formats: vec![] },
                CardFormats { devnum: 0xe201, formats: vec![] },
                CardFormats { devnum: 0xe202, formats: vec![] },
            ],
        };
        // nothing to go on: primary first, the order bring-up found
        assert_eq!(tranche_order(&info, None), vec![0, 1, 2]);
        // a surface composited on the second card promotes it, and the
        // others keep their relative order behind it
        assert_eq!(tranche_order(&info, Some(0xe201)), vec![1, 0, 2]);
        // a device no card owns is not a reason to reshuffle
        assert_eq!(tranche_order(&info, Some(0xdead)), vec![0, 1, 2]);
    }

    #[test]
    fn every_card_gets_its_own_sampling_tranche() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(DmabufInfo {
            cards: vec![
                CardFormats {
                    devnum: 0xe200,
                    formats: vec![(XRGB8888.drm, MOD_LINEAR, 1)],
                },
                CardFormats {
                    devnum: 0xe201,
                    formats: vec![(XRGB8888.drm, 42, 1), (ARGB8888.drm, 42, 1)],
                },
            ],
        });
        feedback(&client, ObjectId(90), 6, None).unwrap();
        let b = client.queued_out_bytes();
        // one table for the whole set, one done at the end
        assert_eq!(count_events(&b, ObjectId(90), 1), 1, "format_table");
        assert_eq!(count_events(&b, ObjectId(90), 0), 1, "done");
        // v6 never sends main_device
        assert_eq!(count_events(&b, ObjectId(90), 2), 0, "main_device");
        // and one full tranche per card
        assert_eq!(count_events(&b, ObjectId(90), 4), 2, "tranche_target_device");
        assert_eq!(count_events(&b, ObjectId(90), 6), 2, "tranche_flags");
        assert_eq!(count_events(&b, ObjectId(90), 5), 2, "tranche_formats");
        assert_eq!(count_events(&b, ObjectId(90), 3), 2, "tranche_done");
        // in card order, each naming its own device
        let devs = event_arrays(&b, ObjectId(90), 4);
        assert_eq!(devs[0], 0xe200u64.to_ne_bytes().to_vec());
        assert_eq!(devs[1], 0xe201u64.to_ne_bytes().to_vec());
        // the second card's indices point past the first card's entries
        let idx = event_arrays(&b, ObjectId(90), 5);
        assert_eq!(idx[0], 0u16.to_ne_bytes().to_vec(), "card0 owns entry 0");
        let mut want = 1u16.to_ne_bytes().to_vec();
        want.extend_from_slice(&2u16.to_ne_bytes());
        assert_eq!(idx[1], want, "card1 owns entries 1 and 2");
    }

    #[test]
    fn identical_feedback_is_not_re_sent() {
        let (state, client) = test_client();
        *state.dmabuf_info.borrow_mut() = Some(DmabufInfo {
            cards: vec![
                CardFormats {
                    devnum: 0xe200,
                    formats: vec![(XRGB8888.drm, MOD_LINEAR, 1)],
                },
                CardFormats {
                    devnum: 0xe201,
                    formats: vec![(XRGB8888.drm, 42, 1)],
                },
            ],
        });
        let fb = Feedback {
            id: ObjectId(90),
            client: client.clone(),
            version: 6,
            last_order: RefCell::new(Vec::new()),
            surface: None,
        };
        send_feedback(&fb, None).unwrap();
        let after_first = client.queued_out_bytes().len();
        assert!(after_first > 0, "first send says something");

        // same card order: the spec asks us not to repeat it
        send_feedback(&fb, None).unwrap();
        assert_eq!(
            client.queued_out_bytes().len(),
            after_first,
            "identical parameters were re-sent"
        );

        // the surface moved to the other card: that is a real change
        send_feedback(&fb, Some(0xe201)).unwrap();
        assert!(
            client.queued_out_bytes().len() > after_first,
            "a new lead card must re-send"
        );
    }
}
