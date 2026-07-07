// zwp-relative-pointer-v1. raw deltas beside the absolute stream - what
// games consume once the pointer is locked. carrot has no accel stage yet,
// so accelerated and unaccelerated are the same numbers.

use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use std::rc::Rc;

pub struct RelativePointerManagerGlobal;

impl Global for RelativePointerManagerGlobal {
    fn interface(&self) -> &'static str {
        zwp_relative_pointer_manager_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(RelativePointerManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct RelativePointerManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl zwp_relative_pointer_manager_v1::Handler for RelativePointerManager {
    fn destroy(
        &self,
        _req: zwp_relative_pointer_manager_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn get_relative_pointer(
        &self,
        req: zwp_relative_pointer_manager_v1::get_relative_pointer::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let rp = Rc::new(RelativePointer {
            id: req.id,
            client: c.clone(),
        });
        c.add_client_obj(rp.clone())?;
        if let Some(seat) = c.state.seat.borrow().clone() {
            seat.add_relative_pointer(c.id, rp);
        }
        Ok(())
    }
}

impl Object for RelativePointerManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_relative_pointer_manager_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_relative_pointer_manager_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct RelativePointer {
    pub id: ObjectId,
    pub client: Rc<Client>,
}

impl zwp_relative_pointer_v1::Handler for RelativePointer {
    fn destroy(
        &self,
        _req: zwp_relative_pointer_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(seat) = self.client.state.seat.borrow().clone() {
            seat.remove_relative_pointer(self.client.id, self.id);
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for RelativePointer {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_relative_pointer_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_relative_pointer_v1::dispatch(&*self, 1, opcode, r)
    }

    fn break_loops(&self) {
        if let Some(seat) = self.client.state.seat.borrow().clone() {
            seat.remove_relative_pointer(self.client.id, self.id);
        }
    }
}
