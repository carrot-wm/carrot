// objects every client starts from: wl_display #1, its registries, callbacks.

use crate::client::{Client, Object};
use crate::protocol::interfaces::{wl_callback, wl_display, wl_registry};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId, WL_DISPLAY_ID};
use std::rc::Rc;

// -- wl_display --

pub struct WlDisplay {
    client: Rc<Client>,
}

impl WlDisplay {
    pub fn new(client: &Rc<Client>) -> WlDisplay {
        WlDisplay {
            client: client.clone(),
        }
    }
}

impl wl_display::Handler for WlDisplay {
    fn sync(&self, req: wl_display::sync::Request) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        c.add_client_obj(Rc::new(WlCallback { id: req.callback }))?;
        // done argument is unspecified, stays 0
        c.event(|o| wl_callback::done::send(o, req.callback, 0));
        c.remove_obj(req.callback)?;
        Ok(())
    }

    fn get_registry(
        &self,
        req: wl_display::get_registry::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let reg = Rc::new(WlRegistry {
            id: req.registry,
            client: c.clone(),
        });
        c.add_client_obj(reg.clone())?;
        c.state.globals.subscribe(&reg);
        Ok(())
    }
}

impl Object for WlDisplay {
    fn id(&self) -> ObjectId {
        WL_DISPLAY_ID
    }

    fn interface(&self) -> &'static str {
        wl_display::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_display::dispatch(&*self, 1, opcode, r)
    }
}

// -- wl_registry --

pub struct WlRegistry {
    pub id: ObjectId,
    client: Rc<Client>,
}

impl WlRegistry {
    pub fn send_global(&self, name: u32, interface: &str, version: u32) {
        self.client
            .event(|o| wl_registry::global::send(o, self.id, name, interface, version));
    }

    pub fn send_global_remove(&self, name: u32) {
        self.client
            .event(|o| wl_registry::global_remove::send(o, self.id, name));
    }
}

impl wl_registry::Handler for WlRegistry {
    fn bind(&self, req: wl_registry::bind::Request) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(g) = c.state.globals.get(req.name) else {
            return Err(format!("global {} does not exist", req.name).into());
        };
        if g.interface() != req.interface {
            return Err(format!(
                "global {} is {}, not {}",
                req.name,
                g.interface(),
                req.interface
            )
            .into());
        }
        if req.version == 0 || req.version > g.version() {
            return Err(format!(
                "global {} only supports versions 1..={}",
                req.name,
                g.version()
            )
            .into());
        }
        g.bind(c, req.id, req.version)?;
        Ok(())
    }
}

impl Object for WlRegistry {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_registry::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_registry::dispatch(&*self, 1, opcode, r)
    }
}

// -- wl_callback --

pub struct WlCallback {
    pub id: ObjectId,
}

impl wl_callback::Handler for WlCallback {}

impl Object for WlCallback {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_callback::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_callback::dispatch(&*self, 1, opcode, r)
    }
}
