// double-buffered surface state: pending until promoted, then the renderer sees it.
// no acquire points yet, so promotion is immediate: commit takes the pending box,
// offers it to the role (sync subsurfaces stash it in their parent), and applies.
// apply_state is the single site where the commit timeline slots in later.

use super::subsurface::WlSubsurface;
use super::{FrameCallback, StackEntry, Transform, WlSurface};
use crate::protocol::shm::AttachedBuffer;
use crate::rect::{Rect, Region};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::mem;
use std::rc::Rc;

pub const MAX_DAMAGE: usize = 32;

#[derive(Default)]
pub struct PendingState {
    /// outer Some = attach requested this cycle; inner None = null buffer (unmap)
    pub buffer: Option<Option<AttachedBuffer>>,
    pub offset: (i32, i32),
    pub opaque_region: Option<Option<Rc<Region>>>,
    pub input_region: Option<Option<Rc<Region>>>,
    pub frame_callbacks: Vec<FrameCallback>,
    pub damage_full: bool,
    pub surface_damage: Vec<Rect>,
    pub buffer_damage: Vec<Rect>,
    pub scale: Option<i32>,
    pub transform: Option<Transform>,
    /// wp_tearing_control hint; Some(false) also carries the revert-on-destroy
    pub tearing: Option<bool>,
    /// cached state of sync children, keyed by subsurface uid, not the reusable wire id
    pub subsurfaces: HashMap<u64, PendingSub>,
}

pub struct PendingSub {
    pub sub: Rc<WlSubsurface>,
    pub state: Option<Box<PendingState>>,
    pub entry: Option<Rc<StackEntry>>,
    pub position: Option<(i32, i32)>,
}

impl PendingSub {
    pub fn new(sub: Rc<WlSubsurface>) -> PendingSub {
        PendingSub {
            sub,
            state: None,
            entry: None,
            position: None,
        }
    }
}

impl PendingState {
    /// repeated commits of a sync subsurface coalesce into one
    pub fn merge(&mut self, mut next: Box<PendingState>) {
        if let Some(b) = next.buffer.take() {
            self.buffer = Some(b);
        }
        self.offset.0 += next.offset.0;
        self.offset.1 += next.offset.1;
        if next.opaque_region.is_some() {
            self.opaque_region = next.opaque_region.take();
        }
        if next.input_region.is_some() {
            self.input_region = next.input_region.take();
        }
        if next.scale.is_some() {
            self.scale = next.scale.take();
        }
        if next.transform.is_some() {
            self.transform = next.transform.take();
        }
        if next.tearing.is_some() {
            self.tearing = next.tearing.take();
        }
        self.frame_callbacks.append(&mut next.frame_callbacks);
        if next.damage_full {
            self.set_full_damage();
        } else {
            self.surface_damage.append(&mut next.surface_damage);
            self.buffer_damage.append(&mut next.buffer_damage);
            self.check_damage_cap();
        }
        for (uid, ps) in next.subsurfaces.drain() {
            match self.subsurfaces.entry(uid) {
                Entry::Occupied(mut e) => {
                    let cur = e.get_mut();
                    if let Some(st) = ps.state {
                        match &mut cur.state {
                            Some(cs) => cs.merge(st),
                            None => cur.state = Some(st),
                        }
                    }
                    if ps.entry.is_some() {
                        cur.entry = ps.entry;
                    }
                    if ps.position.is_some() {
                        cur.position = ps.position;
                    }
                }
                Entry::Vacant(v) => {
                    v.insert(ps);
                }
            }
        }
    }

    pub fn set_full_damage(&mut self) {
        self.damage_full = true;
        self.surface_damage.clear();
        self.buffer_damage.clear();
    }

    pub fn check_damage_cap(&mut self) {
        if self.surface_damage.len() + self.buffer_damage.len() > MAX_DAMAGE {
            self.set_full_damage();
        }
    }

    /// back to as-new, keeping allocations for reuse
    pub fn reset(&mut self) {
        self.buffer = None;
        self.offset = (0, 0);
        self.opaque_region = None;
        self.input_region = None;
        self.frame_callbacks.clear();
        self.damage_full = false;
        self.surface_damage.clear();
        self.buffer_damage.clear();
        self.scale = None;
        self.transform = None;
        self.tearing = None;
        self.subsurfaces.clear();
    }
}

/// copies tight w*4 rows out of the client buffer. false = nothing captured,
/// so the old shadow stays and the release falls to the drop-on-replace path
fn capture_shadow(buf: &crate::protocol::shm::WlBuffer, slot: &mut Option<Vec<u8>>) -> bool {
    use crate::clientmem::ShmAccess;
    let (w, h) = (buf.rect.width() as usize, buf.rect.height() as usize);
    let row = w * 4;
    let need = row * h;
    if need == 0 {
        return false;
    }
    let stride = buf.stride as usize;
    let Some(access) = buf.shm_access() else {
        return false;
    };
    let px = slot.get_or_insert_with(Vec::new);
    px.resize(need, 0);
    match access {
        ShmAccess::Ptr(p) => {
            for yy in 0..h {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        p.add(yy * stride),
                        px[yy * row..].as_mut_ptr(),
                        row,
                    );
                }
            }
        }
        ShmAccess::Fd { fd, offset } => {
            for yy in 0..h {
                let dst = &mut px[yy * row..yy * row + row];
                let mut done = 0;
                while done < row {
                    // short reads zero-fill rather than leaking stale rows
                    match rustix::io::pread(fd, &mut dst[done..], (offset + yy * stride + done) as u64) {
                        Ok(0) | Err(_) => {
                            dst[done..].fill(0);
                            break;
                        }
                        Ok(n) => done += n,
                    }
                }
            }
        }
    }
    true
}

impl WlSurface {
    pub(crate) fn commit_impl(&self) {
        {
            let p = self.pending.borrow();
            if let Some(Some(b)) = &p.buffer {
                // every committed buffer owes a release once replaced or dropped
                b.send_release.set(true);
            }
        }
        let fresh = self.pending_free.pop().unwrap_or_default();
        let taken = self.pending.replace(fresh);
        let ext = self.ext.borrow().clone();
        if let Some(pending) = ext.commit_requested(taken) {
            self.apply_state(pending);
        }
    }

    pub(crate) fn apply_state(&self, mut pending: Box<PendingState>) {
        // sync children first: their cached state, then order, then position
        if !pending.subsurfaces.is_empty() {
            let subs: Vec<_> = pending.subsurfaces.drain().map(|(_, s)| s).collect();
            for ps in subs {
                if let Some(st) = ps.state {
                    ps.sub.surface.apply_state(st);
                }
                if let Some(entry) = ps.entry {
                    entry.pending.set(false);
                    let old = ps.sub.entry.borrow_mut().replace(entry);
                    if let Some(old) = old {
                        ps.sub.parent.remove_stack_entry(&old);
                    }
                }
                if let Some(pos) = ps.position {
                    ps.sub.position.set(pos);
                }
            }
        }
        if self.destroyed.get() {
            return;
        }
        let ext = self.ext.borrow().clone();
        ext.before_apply();
        if let Some(s) = pending.scale.take() {
            self.scale.set(s);
        }
        if let Some(t) = pending.transform.take() {
            self.transform.set(t);
        }
        if let Some(t) = pending.tearing.take() {
            self.tearing.set(t);
        }
        // pixels only change on attach or damage; shm textures re-upload
        // off content_gen, not every frame
        let content_changed = pending.buffer.is_some()
            || pending.damage_full
            || !pending.surface_damage.is_empty()
            || !pending.buffer_damage.is_empty();
        if content_changed {
            self.content_gen.set(self.content_gen.get().wrapping_add(1));
        }
        if let Some(buf) = pending.buffer.take() {
            // dropping the previous attachment sends its release. dmabufs are
            // sampled in place, so their release waits out the frame that may
            // still reference them - the present loop drains the parking lot
            let old = self.buffer.borrow_mut().take();
            if let Some(old) = old {
                let st = &self.client.state;
                if old.buf.dmabuf().is_some() && st.display.borrow().is_some() {
                    st.retired.borrow_mut().push(old);
                }
            }
            match buf {
                Some(b) => {
                    *self.buffer.borrow_mut() = Some(b);
                }
                None => {
                    self.buf_x.set(0);
                    self.buf_y.set(0);
                    self.shm_shadow.borrow_mut().take();
                }
            }
        }
        // shm pixels copy out here and the buffer releases right away -
        // clients single-buffer instead of aging pools, and compositing
        // reads the shadow, never client memory that may be rewritten
        if content_changed {
            let att = self.buffer.borrow();
            if let Some(att) = att.as_ref() {
                if att.buf.dmabuf().is_none()
                    && capture_shadow(&att.buf, &mut self.shm_shadow.borrow_mut())
                {
                    att.send_release.set(false);
                    if !att.buf.destroyed.get() {
                        let b = &att.buf;
                        b.client.event(|o| {
                            crate::protocol::interfaces::wl_buffer::release::send(o, b.id)
                        });
                    }
                }
            }
        }
        let has_buffer = self.buffer.borrow().is_some();
        if has_buffer {
            self.buf_x.set(self.buf_x.get() + pending.offset.0);
            self.buf_y.set(self.buf_y.get() + pending.offset.1);
        }
        // logical size: swap for 90/270 transforms, ceil-div by scale
        let size = match &*self.buffer.borrow() {
            Some(b) => {
                let (mut w, mut h) = (b.buf.rect.width(), b.buf.rect.height());
                if self.transform.get().swaps_dimensions() {
                    mem::swap(&mut w, &mut h);
                }
                let s = self.scale.get().max(1);
                // i32::div_ceil is still unstable; dims are non-negative
                (
                    (w as u32).div_ceil(s as u32) as i32,
                    (h as u32).div_ceil(s as u32) as i32,
                )
            }
            None => (0, 0),
        };
        self.size.set(size);
        // mapping follows the committed buffer; role hooks refine it
        // (a subsurface also needs its parent mapped)
        self.mapped.set(has_buffer);
        self.frame_callbacks
            .borrow_mut()
            .append(&mut pending.frame_callbacks);
        if let Some(r) = pending.input_region.take() {
            *self.input_region.borrow_mut() = r;
        }
        if let Some(r) = pending.opaque_region.take() {
            *self.opaque_region.borrow_mut() = r;
        }
        // damage feeds the renderer, which doesn't exist yet
        pending.surface_damage.clear();
        pending.buffer_damage.clear();
        pending.damage_full = false;
        self.update_extents();
        ext.after_apply();
        crate::trace!(
            "surface {} applied: mapped={} size={:?}",
            self.id,
            self.mapped.get(),
            self.size.get()
        );
        self.client.state.damage.trigger();
        // toplevel captures latch on commits, not presents, so windows on
        // invisible workspaces keep delivering frames
        if content_changed {
            crate::protocol::image_copy_capture::content_changed(&self.client.state, self);
        }
        // recycle the box (the sync-subsurface stash path keeps its own)
        pending.reset();
        self.pending_free.push(pending);
    }
}
