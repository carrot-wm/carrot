// wl_output - one global per connector, registered once the mode is
// known. static answers for now; hotplug rewires this later.

use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::wl_output;
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
        client.add_client_obj(out)?;
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
