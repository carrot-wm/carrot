// zwp_primary_selection_v1 - the middle-click clipboard. same shape as
// wl_data_device: sources carry mimes, the keyboard-focus holder gets a
// fresh offer, receive() forwards the pipe fd to the source owner.

use crate::client::{Client, ClientError, ClientId, Object};
use crate::protocol::data_device::{SelectionSource, same_source};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    zwp_primary_selection_device_manager_v1 as manager, zwp_primary_selection_device_v1 as device,
    zwp_primary_selection_offer_v1 as offer, zwp_primary_selection_source_v1 as source,
};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::state::State;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

// -- the seat-side state --

#[derive(Default)]
pub struct PrimaryDevices {
    devices: RefCell<HashMap<ClientId, Vec<Rc<PrimaryDevice>>>>,
    sources: RefCell<HashMap<(ClientId, u32), Rc<PrimarySource>>>,
    selection: RefCell<Option<Rc<dyn SelectionSource>>>,
}

impl PrimaryDevices {
    pub fn drop_client(&self, state: &Rc<State>, id: ClientId) {
        self.devices.borrow_mut().remove(&id);
        let owned = {
            let sources = self.sources.borrow();
            self.selection.borrow().as_ref().is_some_and(|sel| {
                sources.iter().any(|(k, s)| k.0 == id && same_source(sel, s))
            })
        };
        self.sources.borrow_mut().retain(|k, _| k.0 != id);
        if owned {
            // clear through the loud path so focus and watchers both hear
            self.set_selection_source(state, None);
        }
    }

    pub fn clear(&self) {
        self.devices.borrow_mut().clear();
        self.sources.borrow_mut().clear();
        *self.selection.borrow_mut() = None;
    }

    pub fn current_source(&self) -> Option<Rc<dyn SelectionSource>> {
        self.selection.borrow().clone()
    }

    pub fn set_selection_source(&self, state: &Rc<State>, src: Option<Rc<dyn SelectionSource>>) {
        let old = self.selection.replace(src);
        if let Some(old) = old {
            let same = self
                .selection
                .borrow()
                .as_ref()
                .is_some_and(|s| Rc::ptr_eq(s, &old));
            if !same {
                old.cancelled();
            }
        }
        let focused = state
            .seat
            .borrow()
            .as_ref()
            .and_then(|s| s.kb_focus.borrow().clone());
        if let Some(surface) = focused {
            self.offer_to(&surface.client);
        }
        // windowless watchers track the primary selection too
        crate::protocol::data_control::selection_changed(state, true);
    }

    pub fn offer_to(&self, client: &Rc<Client>) {
        let devices = match self.devices.borrow().get(&client.id) {
            Some(d) => d.clone(),
            None => return,
        };
        let selection = self.selection.borrow().clone();
        for dev in devices {
            match &selection {
                Some(src) => {
                    let id = client.objects.alloc_server_id();
                    let off = Rc::new(PrimaryOffer {
                        id,
                        client: client.clone(),
                        version: dev.version,
                        source: Rc::downgrade(src),
                    });
                    client.add_server_obj(off);
                    client.event(|o| {
                        device::data_offer::send(o, dev.id, id);
                        for mime in src.mimes() {
                            offer::offer::send(o, id, &mime);
                        }
                        device::selection::send(o, dev.id, id);
                    });
                }
                None => {
                    client.event(|o| device::selection::send(o, dev.id, ObjectId::NONE));
                }
            }
        }
    }
}

fn seat(state: &Rc<State>) -> Option<Rc<crate::input::seat::SeatGlobal>> {
    state.seat.borrow().clone()
}

// -- the manager --

pub struct PrimarySelectionGlobal;

impl Global for PrimarySelectionGlobal {
    fn interface(&self) -> &'static str {
        manager::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(PrimaryManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct PrimaryManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl manager::Handler for PrimaryManager {
    fn create_source(
        &self,
        req: manager::create_source::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let src = Rc::new(PrimarySource {
            id: req.id,
            client: self.client.clone(),
            version: self.version,
            mimes: RefCell::new(Vec::new()),
        });
        self.client.add_client_obj(src.clone())?;
        if let Some(seat) = seat(&self.client.state) {
            seat.primary
                .sources
                .borrow_mut()
                .insert((self.client.id, req.id.0), src);
        }
        Ok(())
    }

    fn get_device(&self, req: manager::get_device::Request) -> Result<(), Box<dyn std::error::Error>> {
        let dev = Rc::new(PrimaryDevice {
            id: req.id,
            client: self.client.clone(),
            version: self.version,
        });
        self.client.add_client_obj(dev.clone())?;
        if let Some(seat) = seat(&self.client.state) {
            seat.primary
                .devices
                .borrow_mut()
                .entry(self.client.id)
                .or_default()
                .push(dev);
        }
        Ok(())
    }

    fn destroy(&self, _req: manager::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for PrimaryManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        manager::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        manager::dispatch(&*self, self.version, opcode, r)
    }
}

// -- the source --

pub struct PrimarySource {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    pub mimes: RefCell<Vec<String>>,
}

impl SelectionSource for PrimarySource {
    fn mimes(&self) -> Vec<String> {
        self.mimes.borrow().clone()
    }

    fn send(&self, mime: &str, fd: std::os::fd::OwnedFd) {
        let fd = Rc::new(fd);
        self.client
            .event(|o| source::send::send(o, self.id, mime, fd));
    }

    fn cancelled(&self) {
        self.client.event(|o| source::cancelled::send(o, self.id));
    }
}

impl source::Handler for PrimarySource {
    fn offer(&self, req: source::offer::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.mimes.borrow_mut().push(req.mime_type.to_string());
        Ok(())
    }

    fn destroy(&self, _req: source::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(seat) = seat(&self.client.state) {
            let removed = seat
                .primary
                .sources
                .borrow_mut()
                .remove(&(self.client.id, self.id.0));
            let is_selection = match (&removed, &*seat.primary.selection.borrow()) {
                (Some(r), Some(sel)) => same_source(sel, r),
                _ => false,
            };
            if is_selection {
                // the full clear path notifies watchers, not just the focus
                seat.primary.set_selection_source(&self.client.state, None);
            }
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for PrimarySource {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        source::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        source::dispatch(&*self, self.version, opcode, r)
    }
}

// -- the device --

pub struct PrimaryDevice {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl device::Handler for PrimaryDevice {
    fn set_selection(
        &self,
        req: device::set_selection::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(seat) = seat(&self.client.state) else {
            return Ok(());
        };
        let src: Option<Rc<dyn SelectionSource>> = if req.source == ObjectId::NONE {
            None
        } else {
            let s = seat
                .primary
                .sources
                .borrow()
                .get(&(self.client.id, req.source.0))
                .cloned();
            match s {
                Some(s) => Some(s),
                None => {
                    self.client.invalid_object(req.source);
                    return Ok(());
                }
            }
        };
        seat.primary.set_selection_source(&self.client.state, src);
        // the x bridge follows wl-side changes from here; it installs its
        // own providers via set_selection_source directly, so no loop
        let xw = self.client.state.xwayland.borrow().clone();
        if let Some(xw) = xw {
            xw.queue
                .push(crate::xwayland::XwmEvent::WlSelection { primary: true });
        }
        Ok(())
    }

    fn destroy(&self, _req: device::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(seat) = seat(&self.client.state) {
            if let Some(list) = seat.primary.devices.borrow_mut().get_mut(&self.client.id) {
                list.retain(|d| d.id != self.id);
            }
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for PrimaryDevice {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        device::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        device::dispatch(&*self, self.version, opcode, r)
    }
}

// -- the offer --

pub struct PrimaryOffer {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    source: Weak<dyn SelectionSource>,
}

impl offer::Handler for PrimaryOffer {
    fn receive(&self, req: offer::receive::Request) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(src) = self.source.upgrade() {
            src.send(&req.mime_type, req.fd);
        }
        Ok(())
    }

    fn destroy(&self, _req: offer::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for PrimaryOffer {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        offer::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        offer::dispatch(&*self, self.version, opcode, r)
    }
}
