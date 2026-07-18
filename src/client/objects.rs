// the object table. two id namespaces: clients allocate 1..0xfeffffff, server
// from 0xff000000 up. dispatch clones the Rc out of the map, so the table is
// never borrowed while a handler runs (handlers add/remove objects freely).

use super::ClientError;
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, IdHash, MIN_SERVER_ID, ObjectId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub trait Object {
    fn id(&self) -> ObjectId;
    fn interface(&self) -> &'static str;
    fn version(&self) -> u32 {
        1
    }
    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError>;
    /// sever owning refs so the client's Rc web can collapse
    fn break_loops(&self) {}
}

#[derive(Default)]
pub struct Objects {
    map: RefCell<HashMap<ObjectId, Rc<dyn Object>, IdHash>>,
    /// server-id free bitmap: set bit = offset free for reuse
    free_server: RefCell<Vec<usize>>,
    /// typed side tables for by-id lookups from other interfaces
    surfaces: RefCell<HashMap<ObjectId, Rc<crate::surface::WlSurface>, IdHash>>,
    regions: RefCell<HashMap<ObjectId, Rc<crate::surface::WlRegion>, IdHash>>,
    buffers: RefCell<HashMap<ObjectId, Rc<crate::protocol::shm::WlBuffer>, IdHash>>,
    toplevels: RefCell<HashMap<ObjectId, Rc<crate::shell::xdg::XdgToplevel>, IdHash>>,
    popups: RefCell<HashMap<ObjectId, Rc<crate::shell::xdg::XdgPopup>, IdHash>>,
    outputs: RefCell<HashMap<ObjectId, Rc<crate::protocol::output::WlOutput>, IdHash>>,
    xdg_outputs: RefCell<HashMap<ObjectId, Rc<crate::protocol::output::XdgOutput>, IdHash>>,
    /// client-scoped per spec: any xdg_wm_base bind may use any of them
    positioners: RefCell<HashMap<ObjectId, Rc<crate::shell::xdg::XdgPositioner>, IdHash>>,
    capture_sources:
        RefCell<HashMap<ObjectId, Rc<crate::protocol::image_copy_capture::CaptureSource>, IdHash>>,
}

impl Objects {
    pub fn get(&self, id: ObjectId) -> Option<Rc<dyn Object>> {
        self.map.borrow().get(&id).cloned()
    }

    pub fn add_client_object(&self, obj: Rc<dyn Object>) -> Result<(), ClientError> {
        let id = obj.id();
        if id.0 == 0 || id.0 >= MIN_SERVER_ID {
            return Err(ClientError::ClientIdOutOfBounds(id));
        }
        self.insert_unique(obj)
    }

    pub fn add_server_object(&self, obj: Rc<dyn Object>) {
        let id = obj.id();
        assert!(id.0 >= MIN_SERVER_ID);
        self.insert_unique(obj)
            .expect("server object id collision");
    }

    fn insert_unique(&self, obj: Rc<dyn Object>) -> Result<(), ClientError> {
        let id = obj.id();
        let mut map = self.map.borrow_mut();
        if map.contains_key(&id) {
            return Err(ClientError::IdAlreadyInUse(id));
        }
        map.insert(id, obj);
        Ok(())
    }

    /// lowest free offset first, to keep the id space dense
    pub fn alloc_server_id(&self) -> ObjectId {
        let mut words = self.free_server.borrow_mut();
        for (i, w) in words.iter_mut().enumerate() {
            if *w != 0 {
                let bit = w.trailing_zeros() as usize;
                *w &= !(1 << bit);
                return ObjectId(MIN_SERVER_ID + (i * usize::BITS as usize + bit) as u32);
            }
        }
        words.push(!1usize);
        ObjectId(MIN_SERVER_ID + ((words.len() - 1) * usize::BITS as usize) as u32)
    }

    /// legal, unused client id? lets one-request objects (wl_display.sync)
    /// skip the table round-trip entirely
    pub fn vacant_client_id(&self, id: ObjectId) -> Result<(), ClientError> {
        if id.0 == 0 || id.0 >= MIN_SERVER_ID {
            return Err(ClientError::ClientIdOutOfBounds(id));
        }
        if self.map.borrow().contains_key(&id) {
            return Err(ClientError::IdAlreadyInUse(id));
        }
        Ok(())
    }

    /// removes the entry; the caller (Client::remove_obj) owns delete_id
    pub fn remove(&self, id: ObjectId) -> Result<Rc<dyn Object>, ClientError> {
        let obj = self
            .map
            .borrow_mut()
            .remove(&id)
            .ok_or(ClientError::UnknownObject(id))?;
        if id.0 >= MIN_SERVER_ID {
            let off = (id.0 - MIN_SERVER_ID) as usize;
            let (word, bit) = (off / usize::BITS as usize, off % usize::BITS as usize);
            self.free_server.borrow_mut()[word] |= 1 << bit;
        }
        Ok(obj)
    }

    /// whole-client teardown. break_loops severs object -> client back refs;
    /// no delete_id, the connection is going away.
    pub fn destroy(&self) {
        self.surfaces.borrow_mut().clear();
        self.regions.borrow_mut().clear();
        self.buffers.borrow_mut().clear();
        self.toplevels.borrow_mut().clear();
        self.popups.borrow_mut().clear();
        self.outputs.borrow_mut().clear();
        self.xdg_outputs.borrow_mut().clear();
        self.positioners.borrow_mut().clear();
        self.capture_sources.borrow_mut().clear();
        let objs: Vec<_> = self.map.borrow_mut().drain().map(|(_, o)| o).collect();
        for obj in &objs {
            obj.break_loops();
        }
    }

    // -- typed lookups --

    pub fn track_surface(&self, s: Rc<crate::surface::WlSurface>) {
        self.surfaces.borrow_mut().insert(s.id, s);
    }

    pub fn surface(&self, id: ObjectId) -> Option<Rc<crate::surface::WlSurface>> {
        self.surfaces.borrow().get(&id).cloned()
    }

    pub fn forget_surface(&self, id: ObjectId) {
        self.surfaces.borrow_mut().remove(&id);
    }

    pub fn track_toplevel(&self, t: Rc<crate::shell::xdg::XdgToplevel>) {
        self.toplevels.borrow_mut().insert(t.id, t);
    }

    pub fn toplevel(&self, id: ObjectId) -> Option<Rc<crate::shell::xdg::XdgToplevel>> {
        self.toplevels.borrow().get(&id).cloned()
    }

    pub fn forget_toplevel(&self, id: ObjectId) {
        self.toplevels.borrow_mut().remove(&id);
    }

    pub fn track_positioner(&self, p: Rc<crate::shell::xdg::XdgPositioner>) {
        self.positioners.borrow_mut().insert(p.id, p);
    }

    pub fn positioner(&self, id: ObjectId) -> Option<Rc<crate::shell::xdg::XdgPositioner>> {
        self.positioners.borrow().get(&id).cloned()
    }

    pub fn forget_positioner(&self, id: ObjectId) {
        self.positioners.borrow_mut().remove(&id);
    }

    pub fn track_popup(&self, p: Rc<crate::shell::xdg::XdgPopup>) {
        self.popups.borrow_mut().insert(p.id, p);
    }

    pub fn popup(&self, id: ObjectId) -> Option<Rc<crate::shell::xdg::XdgPopup>> {
        self.popups.borrow().get(&id).cloned()
    }

    pub fn forget_popup(&self, id: ObjectId) {
        self.popups.borrow_mut().remove(&id);
    }

    pub fn track_output(&self, o: Rc<crate::protocol::output::WlOutput>) {
        self.outputs.borrow_mut().insert(o.id, o);
    }

    pub fn for_each_output(&self, mut f: impl FnMut(&Rc<crate::protocol::output::WlOutput>)) {
        for o in self.outputs.borrow().values() {
            f(o);
        }
    }

    pub fn output(&self, id: ObjectId) -> Option<Rc<crate::protocol::output::WlOutput>> {
        self.outputs.borrow().get(&id).cloned()
    }

    pub fn forget_output(&self, id: ObjectId) {
        self.outputs.borrow_mut().remove(&id);
    }

    pub fn track_xdg_output(&self, o: Rc<crate::protocol::output::XdgOutput>) {
        self.xdg_outputs.borrow_mut().insert(o.id, o);
    }

    pub fn for_each_xdg_output(&self, mut f: impl FnMut(&Rc<crate::protocol::output::XdgOutput>)) {
        for o in self.xdg_outputs.borrow().values() {
            f(o);
        }
    }

    pub fn forget_xdg_output(&self, id: ObjectId) {
        self.xdg_outputs.borrow_mut().remove(&id);
    }

    pub fn for_each_surface(&self, mut f: impl FnMut(&Rc<crate::surface::WlSurface>)) {
        for s in self.surfaces.borrow().values() {
            f(s);
        }
    }

    pub fn track_region(&self, r: Rc<crate::surface::WlRegion>) {
        self.regions.borrow_mut().insert(r.id, r);
    }

    pub fn region(&self, id: ObjectId) -> Option<Rc<crate::surface::WlRegion>> {
        self.regions.borrow().get(&id).cloned()
    }

    pub fn forget_region(&self, id: ObjectId) {
        self.regions.borrow_mut().remove(&id);
    }

    pub fn track_buffer(&self, b: Rc<crate::protocol::shm::WlBuffer>) {
        self.buffers.borrow_mut().insert(b.id, b);
    }

    pub fn buffer(&self, id: ObjectId) -> Option<Rc<crate::protocol::shm::WlBuffer>> {
        self.buffers.borrow().get(&id).cloned()
    }

    pub fn forget_buffer(&self, id: ObjectId) {
        self.buffers.borrow_mut().remove(&id);
    }

    pub fn track_capture_source(
        &self,
        s: Rc<crate::protocol::image_copy_capture::CaptureSource>,
    ) {
        self.capture_sources.borrow_mut().insert(s.id, s);
    }

    pub fn capture_source(
        &self,
        id: ObjectId,
    ) -> Option<Rc<crate::protocol::image_copy_capture::CaptureSource>> {
        self.capture_sources.borrow().get(&id).cloned()
    }

    pub fn forget_capture_source(&self, id: ObjectId) {
        self.capture_sources.borrow_mut().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct T(ObjectId);
    impl Object for T {
        fn id(&self) -> ObjectId {
            self.0
        }
        fn interface(&self) -> &'static str {
            "t"
        }
        fn handle_request(
            self: Rc<Self>,
            _: u32,
            _: &mut MsgReader<'_>,
        ) -> Result<(), DispatchError> {
            Ok(())
        }
    }

    #[test]
    fn vacancy_matches_add_semantics() {
        let o = Objects::default();
        assert!(o.vacant_client_id(ObjectId(3)).is_ok());
        assert!(matches!(
            o.vacant_client_id(ObjectId(0)),
            Err(ClientError::ClientIdOutOfBounds(_))
        ));
        assert!(matches!(
            o.vacant_client_id(ObjectId(MIN_SERVER_ID)),
            Err(ClientError::ClientIdOutOfBounds(_))
        ));
        o.add_client_object(Rc::new(T(ObjectId(3)))).unwrap();
        assert!(matches!(
            o.vacant_client_id(ObjectId(3)),
            Err(ClientError::IdAlreadyInUse(_))
        ));
        o.remove(ObjectId(3)).unwrap();
        assert!(o.vacant_client_id(ObjectId(3)).is_ok());
    }
}
