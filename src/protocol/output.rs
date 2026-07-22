// wl_output - one global per connector, registered once the mode is
// known. static answers for now; hotplug rewires this later.

use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{wl_output, zxdg_output_manager_v1, zxdg_output_v1};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use std::rc::Rc;

pub struct WlOutputGlobal {
    pub name: String,
    pub x: i32,
    pub y: i32,
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
            name: self.name.clone(),
        });
        client.add_client_obj(out.clone())?;
        client.objects.track_output(out.clone());
        // a fresh wl_output bind re-enters any workspace group already
        // pinned to this connector for the binding client
        crate::protocol::ext_workspace::output_bound(client, &out);
        client.event(|o| {
            // physical size unknown; 0,0 is the protocol's "don't know"
            wl_output::geometry::send(o, id, self.x, self.y, 0, 0, 0, "carrot", &self.name, 0);
            // flags: current | preferred
            wl_output::mode::send(o, id, 3, self.width, self.height, self.refresh_mhz);
            if version >= 2 {
                wl_output::scale::send(o, id, 1);
            }
            if version >= 4 {
                wl_output::name::send(o, id, &self.name);
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
    /// connector name, e.g. eDP-1
    pub name: String,
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
            name: output.name.clone(),
        });
        c.add_client_obj(xo.clone())?;
        c.objects.track_xdg_output(xo.clone());
        let (x, y, w, h) = logical_of(c, &output.name);
        c.event(|o| {
            zxdg_output_v1::logical_position::send(o, xo.id, x, y);
            zxdg_output_v1::logical_size::send(o, xo.id, w, h);
            if xo.version >= 2 {
                // must match wl_output.name for clients to join the two
                zxdg_output_v1::name::send(o, xo.id, &output.name);
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
    /// connector name; matches the wl_output
    pub name: String,
}

impl zxdg_output_v1::Handler for XdgOutput {
    fn destroy(&self, _req: zxdg_output_v1::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.client.objects.forget_xdg_output(self.id);
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

/// the live global-space rect of the named connector; the union extent
/// when it isn't up (headless probes)
fn logical_of(c: &Rc<Client>, name: &str) -> (i32, i32, i32, i32) {
    let d = c.state.display.borrow();
    let rect = d.as_ref().and_then(|d| {
        d.outputs
            .borrow()
            .iter()
            .find(|o| o.conn.name == name)
            .map(|o| o.rect())
    });
    match rect {
        Some(r) => (r.x1, r.y1, r.width(), r.height()),
        None => {
            let (w, h) = c.state.output_size.get();
            (0, 0, w as i32, h as i32)
        }
    }
}

/// topology moved (hotplug, re-slot): every bound wl_output/xdg_output
/// learns its output's new place, closed by a done each
pub fn resend_output_state(state: &Rc<crate::state::State>) {
    state.clients.for_each(|c| {
        c.objects.for_each_xdg_output(|xo| {
            let (x, y, w, h) = logical_of(&xo.client, &xo.name);
            xo.client.event(|o| {
                zxdg_output_v1::logical_position::send(o, xo.id, x, y);
                zxdg_output_v1::logical_size::send(o, xo.id, w, h);
                if xo.version < 3 {
                    zxdg_output_v1::done::send(o, xo.id);
                }
            });
        });
        c.objects.for_each_output(|out| {
            let (x, y, w, h) = logical_of(&out.client, &out.name);
            let hz = {
                let d = out.client.state.display.borrow();
                d.as_ref()
                    .and_then(|d| {
                        d.outputs
                            .borrow()
                            .iter()
                            .find(|o| o.conn.name == out.name)
                            .and_then(|o| o.conn.pipe.borrow().as_ref().map(|p| p.mode.vrefresh))
                    })
                    .unwrap_or(0)
            };
            out.client.event(|o| {
                wl_output::geometry::send(o, out.id, x, y, 0, 0, 0, "carrot", &out.name, 0);
                // refresh is millihertz on the wire, like the bind path sends
                wl_output::mode::send(o, out.id, 3, w, h, (hz * 1000) as i32);
                if out.version >= 2 {
                    wl_output::done::send(o, out.id);
                }
            });
        });
    });
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
