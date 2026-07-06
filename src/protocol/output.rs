// wl_output - one global per connector, registered once the mode is
// known. static answers for now; hotplug rewires this later.

use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{wl_output, zxdg_output_manager_v1, zxdg_output_v1};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use std::rc::Rc;

pub struct WlOutputGlobal {
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
}

impl Global for WlOutputGlobal {
    fn interface(&self) -> &'static str {
        wl_output::NAME
    }

    fn version(&self) -> u32 {
        4
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        let out = Rc::new(WlOutput {
            id,
            client: client.clone(),
            version,
        });
        client.add_client_obj(out.clone())?;
        client.objects.track_output(out);
        client.event(|o| {
            // physical size unknown; 0,0 is the protocol's "don't know"
            wl_output::geometry::send(o, id, 0, 0, 0, 0, 0, "carrot", "output", 0);
            // flags: current | preferred
            wl_output::mode::send(o, id, 3, self.width, self.height, self.refresh_mhz);
            if version >= 2 {
                wl_output::scale::send(o, id, 1);
            }
            if version >= 4 {
                wl_output::name::send(o, id, "carrot-0");
                wl_output::description::send(o, id, "carrot output");
            }
            if version >= 2 {
                wl_output::done::send(o, id);
            }
        });
        Ok(())
    }
}

pub struct WlOutput {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl wl_output::Handler for WlOutput {
    fn release(&self, _req: wl_output::release::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.client.objects.forget_output(self.id);
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for WlOutput {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_output::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_output::dispatch(&*self, self.version, opcode, r)
    }
}

// -- xdg-output --

pub struct XdgOutputManagerGlobal;

impl Global for XdgOutputManagerGlobal {
    fn interface(&self) -> &'static str {
        zxdg_output_manager_v1::NAME
    }

    fn version(&self) -> u32 {
        3
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(XdgOutputManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct XdgOutputManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl zxdg_output_manager_v1::Handler for XdgOutputManager {
    fn destroy(
        &self,
        _req: zxdg_output_manager_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn get_xdg_output(
        &self,
        req: zxdg_output_manager_v1::get_xdg_output::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(output) = c.objects.output(req.output) else {
            c.invalid_object(req.output);
            return Ok(());
        };
        let xo = Rc::new(XdgOutput {
            id: req.id,
            client: c.clone(),
            version: self.version,
        });
        c.add_client_obj(xo.clone())?;
        let (w, h) = c.state.output_size.get();
        c.event(|o| {
            zxdg_output_v1::logical_position::send(o, xo.id, 0, 0);
            zxdg_output_v1::logical_size::send(o, xo.id, w as i32, h as i32);
            if xo.version >= 2 {
                zxdg_output_v1::name::send(o, xo.id, "carrot-0");
            }
            // v3 atomicity rides the wl_output done; older binds get ours
            if xo.version >= 3 {
                if output.version >= 2 {
                    wl_output::done::send(o, output.id);
                }
            } else {
                zxdg_output_v1::done::send(o, xo.id);
            }
        });
        Ok(())
    }
}

impl Object for XdgOutputManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zxdg_output_manager_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zxdg_output_manager_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct XdgOutput {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl zxdg_output_v1::Handler for XdgOutput {
    fn destroy(&self, _req: zxdg_output_v1::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for XdgOutput {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zxdg_output_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zxdg_output_v1::dispatch(&*self, self.version, opcode, r)
    }
}
