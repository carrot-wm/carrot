// the clipboard: wl_data_device_manager, sources, devices, offers.
//
// selection only for now - a source is set with the copied mime types,
// every keyboard focus change hands the focused client a fresh offer,
// and receive() pipes the fd straight through to the source's owner.
// drag and drop requests are accepted and go nowhere.

use crate::client::{Client, ClientError, ClientId, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::state::State;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

// -- the provider trait --

// whoever owns the selection: a wl source, a primary source, or the
// x11 bridge. offers hold these weakly and pipe receive() through send()
pub trait SelectionSource {
    fn mimes(&self) -> Vec<String>;
    // the receiver hands over the write end; forward the data, drop it
    fn send(&self, mime: &str, fd: std::os::fd::OwnedFd);
    fn cancelled(&self);
    // x-bridge providers die with their server
    fn is_x11(&self) -> bool {
        false
    }
}

// dyn slot vs a concrete source, by allocation
pub(crate) fn same_source<T: SelectionSource + 'static>(
    sel: &Rc<dyn SelectionSource>,
    src: &Rc<T>,
) -> bool {
    let src: Rc<dyn SelectionSource> = src.clone();
    Rc::ptr_eq(sel, &src)
}

// -- the seat-side selection state --

#[derive(Default)]
pub struct DataDevices {
    devices: RefCell<HashMap<ClientId, Vec<Rc<WlDataDevice>>>>,
    // sources by (client, id); set_selection resolves through here
    sources: RefCell<HashMap<(ClientId, u32), Rc<WlDataSource>>>,
    selection: RefCell<Option<Rc<dyn SelectionSource>>>,
}

impl DataDevices {
    pub fn drop_client(&self, id: ClientId) {
        self.devices.borrow_mut().remove(&id);
        let owned = {
            let sources = self.sources.borrow();
            self.selection.borrow().as_ref().is_some_and(|sel| {
                sources.iter().any(|(k, s)| k.0 == id && same_source(sel, s))
            })
        };
        self.sources.borrow_mut().retain(|k, _| k.0 != id);
        if owned {
            *self.selection.borrow_mut() = None;
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
        // the holder of the keyboard learns about the new clipboard now;
        // everyone else on their next focus
        let focused = state
            .seat
            .borrow()
            .as_ref()
            .and_then(|s| s.kb_focus.borrow().clone());
        if let Some(surface) = focused {
            self.offer_to(&surface.client);
        }
    }

    // a fresh wl_data_offer per data device, then selection(offer)
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
                    let offer = Rc::new(WlDataOffer {
                        id,
                        client: client.clone(),
                        version: dev.version,
                        source: Rc::downgrade(src),
                    });
                    client.add_server_obj(offer);
                    client.event(|o| {
                        wl_data_device::data_offer::send(o, dev.id, id);
                        for mime in src.mimes() {
                            wl_data_offer::offer::send(o, id, &mime);
                        }
                        wl_data_device::selection::send(o, dev.id, id);
                    });
                }
                None => {
                    client.event(|o| {
                        wl_data_device::selection::send(o, dev.id, ObjectId::NONE)
                    });
                }
            }
        }
    }
}

fn seat_data(state: &Rc<State>) -> Option<Rc<crate::input::seat::SeatGlobal>> {
    state.seat.borrow().clone()
}

// -- wl_data_device_manager --

pub struct WlDataDeviceManagerGlobal;

impl Global for WlDataDeviceManagerGlobal {
    fn interface(&self) -> &'static str {
        wl_data_device_manager::NAME
    }

    fn version(&self) -> u32 {
        3
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(WlDataDeviceManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct WlDataDeviceManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl wl_data_device_manager::Handler for WlDataDeviceManager {
    fn create_data_source(
        &self,
        req: wl_data_device_manager::create_data_source::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let src = Rc::new(WlDataSource {
            id: req.id,
            client: self.client.clone(),
            version: self.version,
            mimes: RefCell::new(Vec::new()),
        });
        self.client.add_client_obj(src.clone())?;
        if let Some(seat) = seat_data(&self.client.state) {
            seat.data
                .sources
                .borrow_mut()
                .insert((self.client.id, req.id.0), src);
        }
        Ok(())
    }

    fn get_data_device(
        &self,
        req: wl_data_device_manager::get_data_device::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dev = Rc::new(WlDataDevice {
            id: req.id,
            client: self.client.clone(),
            version: self.version,
        });
        self.client.add_client_obj(dev.clone())?;
        if let Some(seat) = seat_data(&self.client.state) {
            seat.data
                .devices
                .borrow_mut()
                .entry(self.client.id)
                .or_default()
                .push(dev);
        }
        Ok(())
    }
}

impl Object for WlDataDeviceManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_data_device_manager::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_data_device_manager::dispatch(&*self, self.version, opcode, r)
    }
}

// -- wl_data_source --

pub struct WlDataSource {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    pub mimes: RefCell<Vec<String>>,
}

impl SelectionSource for WlDataSource {
    fn mimes(&self) -> Vec<String> {
        self.mimes.borrow().clone()
    }

    fn send(&self, mime: &str, fd: std::os::fd::OwnedFd) {
        let fd = Rc::new(fd);
        self.client
            .event(|o| wl_data_source::send::send(o, self.id, mime, fd));
    }

    fn cancelled(&self) {
        self.client
            .event(|o| wl_data_source::cancelled::send(o, self.id));
    }
}

impl wl_data_source::Handler for WlDataSource {
    fn offer(&self, req: wl_data_source::offer::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.mimes.borrow_mut().push(req.mime_type.to_string());
        Ok(())
    }

    fn destroy(&self, _req: wl_data_source::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(seat) = seat_data(&self.client.state) {
            let removed = seat
                .data
                .sources
                .borrow_mut()
                .remove(&(self.client.id, self.id.0));
            // destroying the live selection unsets it
            let is_selection = match (&removed, &*seat.data.selection.borrow()) {
                (Some(r), Some(sel)) => same_source(sel, r),
                _ => false,
            };
            if is_selection {
                *seat.data.selection.borrow_mut() = None;
                let focused = seat.kb_focus.borrow().clone();
                if let Some(surface) = focused {
                    seat.data.offer_to(&surface.client);
                }
            }
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn set_actions(
        &self,
        _req: wl_data_source::set_actions::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
}

impl Object for WlDataSource {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_data_source::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_data_source::dispatch(&*self, self.version, opcode, r)
    }
}

// -- wl_data_device --

pub struct WlDataDevice {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl wl_data_device::Handler for WlDataDevice {
    fn start_drag(
        &self,
        _req: wl_data_device::start_drag::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // no dnd yet; a drag that never enters anything is legal
        Ok(())
    }

    fn set_selection(
        &self,
        req: wl_data_device::set_selection::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(seat) = seat_data(&self.client.state) else {
            return Ok(());
        };
        let source: Option<Rc<dyn SelectionSource>> = if req.source == ObjectId::NONE {
            None
        } else {
            let src = seat
                .data
                .sources
                .borrow()
                .get(&(self.client.id, req.source.0))
                .cloned();
            match src {
                Some(s) => Some(s),
                None => {
                    self.client.invalid_object(req.source);
                    return Ok(());
                }
            }
        };
        seat.data.set_selection_source(&self.client.state, source);
        // the x bridge follows wl-side changes from here; it installs its
        // own providers via set_selection_source directly, so no loop
        let xw = self.client.state.xwayland.borrow().clone();
        if let Some(xw) = xw {
            xw.queue
                .push(crate::xwayland::XwmEvent::WlSelection { primary: false });
        }
        Ok(())
    }

    fn release(&self, _req: wl_data_device::release::Request) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(seat) = seat_data(&self.client.state) {
            if let Some(list) = seat.data.devices.borrow_mut().get_mut(&self.client.id) {
                list.retain(|d| d.id != self.id);
            }
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for WlDataDevice {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_data_device::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_data_device::dispatch(&*self, self.version, opcode, r)
    }
}

// -- wl_data_offer --

pub struct WlDataOffer {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    source: Weak<dyn SelectionSource>,
}

impl wl_data_offer::Handler for WlDataOffer {
    fn accept(&self, _req: wl_data_offer::accept::Request) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn receive(&self, req: wl_data_offer::receive::Request) -> Result<(), Box<dyn std::error::Error>> {
        // hand the pipe's write end to the source owner; dropping it on a
        // dead source closes it and the reader sees eof
        if let Some(src) = self.source.upgrade() {
            src.send(&req.mime_type, req.fd);
        }
        Ok(())
    }

    fn destroy(&self, _req: wl_data_offer::destroy::Request) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn finish(&self, _req: wl_data_offer::finish::Request) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn set_actions(
        &self,
        _req: wl_data_offer::set_actions::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
}

impl Object for WlDataOffer {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wl_data_offer::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wl_data_offer::dispatch(&*self, self.version, opcode, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use crate::protocol::MIN_SERVER_ID;
    use crate::surface::WlSurface;
    use wl_data_device::Handler as _;
    use wl_data_device_manager::Handler as _;
    use wl_data_offer::Handler as _;
    use wl_data_source::Handler as _;

    fn setup() -> (
        Rc<State>,
        Rc<Client>,
        Rc<crate::input::seat::SeatGlobal>,
        Rc<WlDataDevice>,
        Rc<WlDataSource>,
    ) {
        let (state, client) = test_client();
        let seat = crate::input::seat::SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat.clone());
        let mgr = Rc::new(WlDataDeviceManager {
            id: ObjectId(60),
            client: client.clone(),
            version: 3,
        });
        client.add_client_obj(mgr.clone()).unwrap();
        mgr.get_data_device(wl_data_device_manager::get_data_device::Request {
            id: ObjectId(61),
            seat: ObjectId(9),
        })
        .unwrap();
        mgr.create_data_source(wl_data_device_manager::create_data_source::Request {
            id: ObjectId(62),
        })
        .unwrap();
        let dev = seat.data.devices.borrow()[&client.id][0].clone();
        let src = seat.data.sources.borrow()[&(client.id, 62)].clone();
        src.offer(wl_data_source::offer::Request {
            mime_type: "text/plain;charset=utf-8".to_string(),
        })
        .unwrap();
        // the focused surface's client is who offers go to
        let s = WlSurface::new(ObjectId(10), &client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        *seat.kb_focus.borrow_mut() = Some(s);
        (state, client, seat, dev, src)
    }

    #[test]
    fn selection_reaches_the_focused_client() {
        let (_state, client, _seat, dev, _src) = setup();
        dev.set_selection(wl_data_device::set_selection::Request {
            source: ObjectId(62),
            serial: 1,
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        let offer_id = ObjectId(MIN_SERVER_ID);
        assert_eq!(count_events(&bytes, dev.id, 0), 1, "data_offer");
        assert_eq!(count_events(&bytes, offer_id, 0), 1, "offer(mime)");
        assert_eq!(count_events(&bytes, dev.id, 5), 1, "selection");
    }

    #[test]
    fn receive_pipes_to_the_source() {
        let (_state, client, _seat, dev, src) = setup();
        dev.set_selection(wl_data_device::set_selection::Request {
            source: ObjectId(62),
            serial: 1,
        })
        .unwrap();
        let dyn_src: Rc<dyn SelectionSource> = src.clone();
        let offer = Rc::new(WlDataOffer {
            id: ObjectId(MIN_SERVER_ID),
            client: client.clone(),
            version: 3,
            source: Rc::downgrade(&dyn_src),
        });
        let fd = rustix::event::eventfd(0, rustix::event::EventfdFlags::empty()).unwrap();
        offer
            .receive(wl_data_offer::receive::Request {
                mime_type: "text/plain;charset=utf-8".to_string(),
                fd,
            })
            .unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, src.id, 1), 1, "source.send");
    }

    #[test]
    fn replacing_the_selection_cancels_the_old_source() {
        let (_state, client, seat, dev, src) = setup();
        dev.set_selection(wl_data_device::set_selection::Request {
            source: ObjectId(62),
            serial: 1,
        })
        .unwrap();
        dev.set_selection(wl_data_device::set_selection::Request {
            source: ObjectId::NONE,
            serial: 2,
        })
        .unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, src.id, 2), 1, "cancelled");
        // two selection events: the offer, then the null
        assert_eq!(count_events(&bytes, dev.id, 5), 2);
        assert!(seat.data.selection.borrow().is_none());
    }

    struct DummySource;

    impl SelectionSource for DummySource {
        fn mimes(&self) -> Vec<String> {
            vec!["text/plain".to_string()]
        }
        fn send(&self, _mime: &str, _fd: std::os::fd::OwnedFd) {}
        fn cancelled(&self) {}
    }

    #[test]
    fn a_dyn_provider_installs_without_xwayland() {
        let (state, client, seat, dev, _src) = setup();
        seat.data
            .set_selection_source(&state, Some(Rc::new(DummySource)));
        assert!(seat.data.selection.borrow().is_some());
        // the focused client got an offer for the dummy's mime
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, dev.id, 0), 1, "data_offer");
        assert_eq!(count_events(&bytes, dev.id, 5), 1, "selection");
    }
}
