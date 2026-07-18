// the clipboard and dnd: wl_data_device_manager, sources, devices, offers.
//
// selection: a source is set with the copied mime types, every keyboard
// focus change hands the focused client a fresh offer, and receive()
// pipes the fd straight through to the source's owner.
//
// dnd: start_drag rides the implicit pointer grab. while it holds, the
// pointer stream becomes data_device enter/leave/motion on the surface
// under the cursor. releasing the grab drops if the target accepted a
// mime (and, v3+, an action survived negotiation), else cancels. a
// source-less drag stays inside the initiating client. x11 dnd bridging
// is xwayland's own business.

use crate::client::{Client, ClientError, ClientId, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, Fixed, ObjectId};
use crate::state::State;
use crate::surface::WlSurface;
use std::cell::{Cell, RefCell};
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

// -- dnd actions (wl_data_device_manager.dnd_action) --

pub const DND_COPY: u32 = 1;
pub const DND_MOVE: u32 = 2;
pub const DND_ASK: u32 = 4;

// -- an in-flight drag session --

// lives on the seat while the grab button is held; after a drop the
// target's offers keep it alive until finish()/destroy conclude it
pub struct Drag {
    pub client: Rc<Client>,
    pub source: Option<Rc<WlDataSource>>,
    pub icon: RefCell<Option<Rc<WlSurface>>>,
    /// wl_surface.offset sum; shifts the icon relative to the pointer
    pub icon_off: Cell<(i32, i32)>,
    target: RefCell<Option<Rc<WlSurface>>>,
    // the offers minted for the current target, one per data device
    offers: RefCell<Vec<Rc<WlDataOffer>>>,
    accepted: Cell<bool>,
    dest_actions: Cell<u32>,
    preferred: Cell<u32>,
    action: Cell<u32>,
    // any v3 device on the target: drop validity then needs an action
    dest_v3: Cell<bool>,
    dropped: Cell<bool>,
}

impl Drag {
    pub fn target(&self) -> Option<Rc<WlSurface>> {
        self.target.borrow().clone()
    }

    // intersect both sides, prefer the target's pick, else copy>move>ask
    fn update_action(&self) {
        let Some(src) = &self.source else { return };
        let both = src.dnd_actions.get() & self.dest_actions.get();
        let preferred = self.preferred.get();
        let chosen = if both & preferred != 0 {
            preferred
        } else if both & DND_COPY != 0 {
            DND_COPY
        } else if both & DND_MOVE != 0 {
            DND_MOVE
        } else if both & DND_ASK != 0 {
            DND_ASK
        } else {
            0
        };
        if self.action.replace(chosen) == chosen {
            return;
        }
        for offer in self.offers.borrow().iter() {
            if offer.version >= wl_data_offer::action::SINCE {
                offer
                    .client
                    .event(|o| wl_data_offer::action::send(o, offer.id, chosen));
            }
        }
        if src.version >= wl_data_source::action::SINCE {
            src.client
                .event(|o| wl_data_source::action::send(o, src.id, chosen));
        }
    }

    fn unlink_offers(&self) {
        for o in self.offers.borrow_mut().drain(..) {
            o.drag.borrow_mut().take();
        }
    }
}

// -- the seat-side selection state --

#[derive(Default)]
pub struct DataDevices {
    devices: RefCell<HashMap<ClientId, Vec<Rc<WlDataDevice>>>>,
    // sources by (client, id); set_selection resolves through here
    sources: RefCell<HashMap<(ClientId, u32), Rc<WlDataSource>>>,
    selection: RefCell<Option<Rc<dyn SelectionSource>>>,
    drag: RefCell<Option<Rc<Drag>>>,
}

impl DataDevices {
    pub fn drop_client(&self, state: &Rc<State>, id: ClientId) {
        if let Some(drag) = self.drag() {
            if drag.client.id == id {
                self.dnd_teardown(false);
            } else if drag.target.borrow().as_ref().is_some_and(|t| t.client.id == id) {
                drag.target.borrow_mut().take();
                drag.unlink_offers();
            }
        }
        self.devices.borrow_mut().remove(&id);
        let owned = {
            let sources = self.sources.borrow();
            self.selection.borrow().as_ref().is_some_and(|sel| {
                sources.iter().any(|(k, s)| k.0 == id && same_source(sel, s))
            })
        };
        self.sources.borrow_mut().retain(|k, _| k.0 != id);
        if owned {
            // the dead owner's clipboard clears loudly: the focus holder
            // gets selection(nil) and watchers see the change
            self.set_selection_source(state, None);
        }
    }

    pub fn clear(&self) {
        self.devices.borrow_mut().clear();
        self.sources.borrow_mut().clear();
        *self.selection.borrow_mut() = None;
        *self.drag.borrow_mut() = None;
    }

    pub fn drag(&self) -> Option<Rc<Drag>> {
        self.drag.borrow().clone()
    }

    pub fn begin_drag_session(&self, drag: Rc<Drag>) {
        *self.drag.borrow_mut() = Some(drag);
    }

    fn client_devices(&self, id: ClientId) -> Vec<Rc<WlDataDevice>> {
        self.devices.borrow().get(&id).cloned().unwrap_or_default()
    }

    /// fresh offers for the new target, then enter
    pub fn dnd_enter(&self, state: &Rc<State>, surface: &Rc<WlSurface>, sx: f64, sy: f64) {
        let Some(drag) = self.drag() else { return };
        let client = &surface.client;
        drag.target.replace(Some(surface.clone()));
        drag.accepted.set(false);
        drag.dest_actions.set(0);
        drag.preferred.set(0);
        drag.action.set(0);
        drag.dest_v3.set(false);
        let devices = self.client_devices(client.id);
        if devices.is_empty() {
            return;
        }
        let serial = state.next_serial(Some(client)) as u32;
        let (fx, fy) = (Fixed::from_f64(sx), Fixed::from_f64(sy));
        let mut offers = Vec::new();
        for dev in devices {
            let offer_id = match &drag.source {
                Some(src) => {
                    let id = client.objects.alloc_server_id();
                    let dyn_src: Rc<dyn SelectionSource> = src.clone();
                    let offer = Rc::new(WlDataOffer {
                        id,
                        client: client.clone(),
                        version: dev.version,
                        source: Rc::downgrade(&dyn_src),
                        drag: RefCell::new(Some(drag.clone())),
                    });
                    client.add_server_obj(offer.clone());
                    client.event(|o| {
                        wl_data_device::data_offer::send(o, dev.id, id);
                        for mime in src.mimes.borrow().iter() {
                            wl_data_offer::offer::send(o, id, mime);
                        }
                        if dev.version >= wl_data_offer::source_actions::SINCE {
                            wl_data_offer::source_actions::send(o, id, src.dnd_actions.get());
                        }
                    });
                    if dev.version >= wl_data_offer::action::SINCE {
                        drag.dest_v3.set(true);
                    }
                    offers.push(offer);
                    id
                }
                None => ObjectId::NONE,
            };
            client.event(|o| {
                wl_data_device::enter::send(o, dev.id, serial, surface.id, fx, fy, offer_id)
            });
        }
        *drag.offers.borrow_mut() = offers;
    }

    pub fn dnd_motion(&self, time_ms: u32, sx: f64, sy: f64) {
        let Some(drag) = self.drag() else { return };
        let Some(target) = drag.target() else { return };
        if target.destroyed.get() {
            return;
        }
        let (fx, fy) = (Fixed::from_f64(sx), Fixed::from_f64(sy));
        for dev in self.client_devices(target.client.id) {
            target
                .client
                .event(|o| wl_data_device::motion::send(o, dev.id, time_ms, fx, fy));
        }
    }

    pub fn dnd_leave(&self) {
        let Some(drag) = self.drag() else { return };
        let Some(target) = drag.target.borrow_mut().take() else { return };
        drag.unlink_offers();
        drag.accepted.set(false);
        drag.action.set(0);
        if target.destroyed.get() {
            return;
        }
        for dev in self.client_devices(target.client.id) {
            target
                .client
                .event(|o| wl_data_device::leave::send(o, dev.id));
        }
    }

    /// the grab released: drop onto an accepting target, else cancel.
    /// clears the seat-side session either way; a dropped session lives
    /// on inside the target's offers until finish()/destroy
    pub fn dnd_finish_session(&self) {
        let Some(drag) = self.drag.borrow_mut().take() else { return };
        let target = drag.target().filter(|t| !t.destroyed.get());
        let valid = target.is_some()
            && match &drag.source {
                None => true,
                Some(_) => drag.accepted.get() && (!drag.dest_v3.get() || drag.action.get() != 0),
            };
        if !valid {
            if let Some(t) = &target {
                for dev in self.client_devices(t.client.id) {
                    t.client.event(|o| wl_data_device::leave::send(o, dev.id));
                }
            }
            drag.unlink_offers();
            if let Some(src) = &drag.source {
                src.cancelled();
            }
            return;
        }
        let t = target.unwrap();
        drag.dropped.set(true);
        // the offers keep their one-way ref for finish(); dropping ours
        // breaks the rc cycle so a vanishing client can't leak the session
        drag.offers.borrow_mut().clear();
        for dev in self.client_devices(t.client.id) {
            t.client.event(|o| wl_data_device::drop::send(o, dev.id));
        }
        if let Some(src) = &drag.source {
            if src.version >= wl_data_source::dnd_drop_performed::SINCE {
                src.client
                    .event(|o| wl_data_source::dnd_drop_performed::send(o, src.id));
            }
        }
    }

    // the drag died out of band (client gone, source destroyed);
    // notify_source is off when the source itself is what died
    fn dnd_teardown(&self, notify_source: bool) {
        let Some(drag) = self.drag.borrow_mut().take() else { return };
        if let Some(t) = drag.target().filter(|t| !t.destroyed.get()) {
            for dev in self.client_devices(t.client.id) {
                t.client.event(|o| wl_data_device::leave::send(o, dev.id));
            }
        }
        drag.unlink_offers();
        if notify_source {
            if let Some(src) = &drag.source {
                src.cancelled();
            }
        }
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
        // windowless watchers hanging off wl_data_control
        crate::protocol::data_control::selection_changed(state, false);
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
                        drag: RefCell::new(None),
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
            dnd_actions: Cell::new(0),
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
    pub dnd_actions: Cell<u32>,
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
            // destroying the source of a live drag cancels the drag
            let dragging = seat.data.drag().is_some_and(|d| {
                d.source
                    .as_ref()
                    .is_some_and(|s| s.client.id == self.client.id && s.id == self.id)
            });
            if dragging {
                seat.data.dnd_teardown(false);
            }
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
                // the full clear path: focus holder gets selection(nil)
                // and windowless watchers hear about it too
                seat.data.set_selection_source(&self.client.state, None);
            }
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn set_actions(
        &self,
        req: wl_data_source::set_actions::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.dnd_actions.set(req.dnd_actions);
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
        req: wl_data_device::start_drag::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(seat) = seat_data(&c.state) else {
            return Ok(());
        };
        let Some(origin) = c.objects.surface(req.origin) else {
            c.invalid_object(req.origin);
            return Ok(());
        };
        // the drag rides the implicit grab named by the serial; anything
        // stale is silently ignored, per spec
        if !seat.drag_grab_valid(&origin, req.serial) {
            return Ok(());
        }
        let source = if req.source == ObjectId::NONE {
            None
        } else {
            let src = seat
                .data
                .sources
                .borrow()
                .get(&(c.id, req.source.0))
                .cloned();
            match src {
                Some(s) => Some(s),
                None => {
                    c.invalid_object(req.source);
                    return Ok(());
                }
            }
        };
        let icon = if req.icon == ObjectId::NONE {
            None
        } else {
            let Some(s) = c.objects.surface(req.icon) else {
                c.invalid_object(req.icon);
                return Ok(());
            };
            match s.role.get() {
                crate::surface::SurfaceRole::None | crate::surface::SurfaceRole::DndIcon => {}
                other => {
                    c.protocol_error(
                        self.id,
                        0, // wl_data_device error role
                        &format!("the icon surface already has the {} role", other.name()),
                    );
                    return Ok(());
                }
            }
            s.role.set(crate::surface::SurfaceRole::DndIcon);
            *s.ext.borrow_mut() = Rc::new(DndIconExt {
                seat: Rc::downgrade(&seat),
                state: Rc::downgrade(&c.state),
                key: (c.id, s.id),
            });
            Some(s)
        };
        let drag = Rc::new(Drag {
            client: c.clone(),
            source,
            icon: RefCell::new(icon),
            icon_off: Cell::new((0, 0)),
            target: RefCell::new(None),
            offers: RefCell::new(Vec::new()),
            accepted: Cell::new(false),
            dest_actions: Cell::new(0),
            preferred: Cell::new(0),
            action: Cell::new(0),
            dest_v3: Cell::new(false),
            dropped: Cell::new(false),
        });
        seat.begin_drag(&c.state, drag);
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

// -- the drag icon role --

// tracks wl_surface.offset shifts and detaches from the drag on death
struct DndIconExt {
    seat: Weak<crate::input::seat::SeatGlobal>,
    state: Weak<State>,
    key: (ClientId, ObjectId),
}

impl DndIconExt {
    fn my_drag(&self) -> Option<Rc<Drag>> {
        let seat = self.seat.upgrade()?;
        let drag = seat.data.drag()?;
        let mine = drag
            .icon
            .borrow()
            .as_ref()
            .is_some_and(|i| (i.client.id, i.id) == self.key);
        mine.then_some(drag)
    }
}

impl crate::surface::SurfaceExt for DndIconExt {
    fn commit_requested(
        self: Rc<Self>,
        pending: Box<crate::surface::PendingState>,
    ) -> Option<Box<crate::surface::PendingState>> {
        if let Some(drag) = self.my_drag() {
            let (x, y) = drag.icon_off.get();
            drag.icon_off
                .set((x + pending.offset.0, y + pending.offset.1));
        }
        Some(pending)
    }

    fn after_apply(&self) {
        // the icon's first buffer usually lands after start_drag
        if self.my_drag().is_some() {
            if let Some(state) = self.state.upgrade() {
                state.damage.trigger();
            }
        }
    }

    fn on_surface_destroy(&self) -> Result<(), ()> {
        if let Some(drag) = self.my_drag() {
            drag.icon.borrow_mut().take();
            if let Some(state) = self.state.upgrade() {
                state.damage.trigger();
            }
        }
        Ok(())
    }
}

// -- wl_data_offer --

pub struct WlDataOffer {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    source: Weak<dyn SelectionSource>,
    // set while this offer belongs to a dnd session; None for selections
    drag: RefCell<Option<Rc<Drag>>>,
}

impl wl_data_offer::Handler for WlDataOffer {
    fn accept(&self, req: wl_data_offer::accept::Request) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(drag) = self.drag.borrow().clone() {
            if !drag.dropped.get() {
                drag.accepted.set(req.mime_type.is_some());
                if let Some(src) = &drag.source {
                    src.client.event(|o| {
                        wl_data_source::target::send(o, src.id, req.mime_type.as_deref())
                    });
                }
            }
        }
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
        // dropping the offer after a drop but before finish aborts the
        // transfer; the source learns via cancelled (v3 targets only -
        // older ones legitimately end with a destroy)
        if let Some(drag) = self.drag.borrow_mut().take() {
            drag.offers
                .borrow_mut()
                .retain(|o| !(o.id == self.id && o.client.id == self.client.id));
            if drag.dropped.get() && self.version >= wl_data_offer::finish::SINCE {
                if let Some(src) = &drag.source {
                    src.cancelled();
                }
            }
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn finish(&self, _req: wl_data_offer::finish::Request) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(drag) = self.drag.borrow_mut().take() {
            if drag.dropped.get() {
                if let Some(src) = &drag.source {
                    if src.version >= wl_data_source::dnd_finished::SINCE {
                        src.client
                            .event(|o| wl_data_source::dnd_finished::send(o, src.id));
                    }
                }
            }
        }
        Ok(())
    }

    fn set_actions(
        &self,
        req: wl_data_offer::set_actions::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(drag) = self.drag.borrow().clone() {
            if !drag.dropped.get() {
                drag.dest_actions.set(req.dnd_actions);
                drag.preferred.set(req.preferred_action);
                drag.update_action();
            }
        }
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
    fn a_dead_owners_selection_clears_for_the_survivor() {
        use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
        let (state, client, seat, dev, _src) = setup();
        dev.set_selection(wl_data_device::set_selection::Request {
            source: ObjectId(62),
            serial: 1,
        })
        .unwrap();
        // a second client holds the focus when the owner goes away
        let (a, _keep) =
            socketpair(AddressFamily::UNIX, SocketType::STREAM, SocketFlags::CLOEXEC, None)
                .unwrap();
        let survivor = state.clients.spawn(&state, a).unwrap();
        let mgr = Rc::new(WlDataDeviceManager {
            id: ObjectId(60),
            client: survivor.clone(),
            version: 3,
        });
        survivor.add_client_obj(mgr.clone()).unwrap();
        mgr.get_data_device(wl_data_device_manager::get_data_device::Request {
            id: ObjectId(61),
            seat: ObjectId(9),
        })
        .unwrap();
        let dev2 = seat.data.devices.borrow()[&survivor.id][0].clone();
        let s2 = WlSurface::new(ObjectId(10), &survivor, 6);
        survivor.add_client_obj(s2.clone()).unwrap();
        survivor.objects.track_surface(s2.clone());
        *seat.kb_focus.borrow_mut() = Some(s2);
        let before = count_events(&survivor.queued_out_bytes(), dev2.id, 5);
        seat.drop_client(&state, client.id);
        let after = count_events(&survivor.queued_out_bytes(), dev2.id, 5);
        assert_eq!(after - before, 1, "the survivor heard selection(nil)");
        assert!(seat.data.current_source().is_none());
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
            drag: RefCell::new(None),
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

    fn test_drag(client: &Rc<Client>, src: Option<Rc<WlDataSource>>) -> Rc<Drag> {
        Rc::new(Drag {
            client: client.clone(),
            source: src,
            icon: RefCell::new(None),
            icon_off: Cell::new((0, 0)),
            target: RefCell::new(None),
            offers: RefCell::new(Vec::new()),
            accepted: Cell::new(false),
            dest_actions: Cell::new(0),
            preferred: Cell::new(0),
            action: Cell::new(0),
            dest_v3: Cell::new(false),
            dropped: Cell::new(false),
        })
    }

    #[test]
    fn dnd_enter_accept_drop_finish() {
        let (state, client, seat, dev, src) = setup();
        src.set_actions(wl_data_source::set_actions::Request {
            dnd_actions: DND_COPY | DND_MOVE,
        })
        .unwrap();
        let surface = seat.kb_focus.borrow().clone().unwrap();
        let drag = test_drag(&client, Some(src.clone()));
        seat.data.begin_drag_session(drag.clone());
        seat.data.dnd_enter(&state, &surface, 5.0, 7.0);
        let bytes = client.queued_out_bytes();
        let offer_id = ObjectId(MIN_SERVER_ID);
        assert_eq!(count_events(&bytes, dev.id, 0), 1, "data_offer");
        assert_eq!(count_events(&bytes, offer_id, 0), 1, "offer(mime)");
        assert_eq!(count_events(&bytes, offer_id, 1), 1, "source_actions");
        assert_eq!(count_events(&bytes, dev.id, 1), 1, "enter");
        // the target accepts and negotiates copy
        let offer = drag.offers.borrow()[0].clone();
        offer
            .accept(wl_data_offer::accept::Request {
                serial: 0,
                mime_type: Some("text/plain;charset=utf-8".to_string()),
            })
            .unwrap();
        offer
            .set_actions(wl_data_offer::set_actions::Request {
                dnd_actions: DND_COPY,
                preferred_action: DND_COPY,
            })
            .unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, src.id, 0), 1, "source.target");
        assert_eq!(count_events(&bytes, src.id, 5), 1, "source.action");
        assert_eq!(count_events(&bytes, offer_id, 2), 1, "offer.action");
        assert_eq!(drag.action.get(), DND_COPY);
        // grab release: drop, then the target finishes
        seat.data.dnd_finish_session();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, dev.id, 4), 1, "drop");
        assert_eq!(count_events(&bytes, src.id, 3), 1, "dnd_drop_performed");
        assert!(seat.data.drag().is_none());
        offer.finish(wl_data_offer::finish::Request {}).unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, src.id, 4), 1, "dnd_finished");
    }

    #[test]
    fn an_unaccepted_drag_cancels_on_release() {
        let (state, client, seat, dev, src) = setup();
        src.set_actions(wl_data_source::set_actions::Request {
            dnd_actions: DND_COPY,
        })
        .unwrap();
        let surface = seat.kb_focus.borrow().clone().unwrap();
        let drag = test_drag(&client, Some(src.clone()));
        seat.data.begin_drag_session(drag);
        seat.data.dnd_enter(&state, &surface, 0.0, 0.0);
        seat.data.dnd_finish_session();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, dev.id, 2), 1, "leave");
        assert_eq!(count_events(&bytes, dev.id, 4), 0, "no drop");
        assert_eq!(count_events(&bytes, src.id, 2), 1, "cancelled");
        assert!(seat.data.drag().is_none());
    }

    #[test]
    fn leaving_the_target_detaches_its_offers() {
        let (state, client, seat, _dev, src) = setup();
        let surface = seat.kb_focus.borrow().clone().unwrap();
        let drag = test_drag(&client, Some(src));
        seat.data.begin_drag_session(drag.clone());
        seat.data.dnd_enter(&state, &surface, 0.0, 0.0);
        let offer = drag.offers.borrow()[0].clone();
        seat.data.dnd_leave();
        assert!(drag.target().is_none());
        assert!(offer.drag.borrow().is_none());
        // a stale accept after leave is inert
        offer
            .accept(wl_data_offer::accept::Request {
                serial: 0,
                mime_type: Some("text/plain".to_string()),
            })
            .unwrap();
        assert!(!drag.accepted.get());
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
