// zwp-pointer-constraints-v1. one constraint per surface; it activates while
// that surface holds pointer focus. lock freezes the cursor (games), confine
// clamps it into a region. oneshot constraints die on deactivation,
// persistent ones re-arm on the next focus.

use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    zwp_confined_pointer_v1, zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::surface::WlSurface;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const ERR_ALREADY_CONSTRAINED: u32 = 1;
const LIFETIME_ONESHOT: u32 = 1;
const LIFETIME_PERSISTENT: u32 = 2;

#[derive(Copy, Clone, PartialEq)]
pub enum Kind {
    Lock,
    Confine,
}

pub struct Constraint {
    pub kind: Kind,
    pub oneshot: bool,
    pub surface: Rc<WlSurface>,
    /// the zwp_locked/confined object, for events
    pub obj: ObjectId,
    pub client: Rc<Client>,
    pub active: Cell<bool>,
    /// a deactivated oneshot never comes back
    pub dead: Cell<bool>,
    pub hint: Cell<Option<(f64, f64)>>,
    pub region: RefCell<Option<Rc<crate::rect::Region>>>,
}

impl Constraint {
    pub fn send_active(&self, active: bool) {
        if self.active.replace(active) == active {
            return;
        }
        let id = self.obj;
        self.client.event(|o| match (self.kind, active) {
            (Kind::Lock, true) => zwp_locked_pointer_v1::locked::send(o, id),
            (Kind::Lock, false) => zwp_locked_pointer_v1::unlocked::send(o, id),
            (Kind::Confine, true) => zwp_confined_pointer_v1::confined::send(o, id),
            (Kind::Confine, false) => zwp_confined_pointer_v1::unconfined::send(o, id),
        });
        if !active && self.oneshot {
            self.dead.set(true);
        }
    }
}

pub struct PointerConstraintsGlobal;

impl Global for PointerConstraintsGlobal {
    fn interface(&self) -> &'static str {
        zwp_pointer_constraints_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(ConstraintsManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct ConstraintsManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl ConstraintsManager {
    fn create(
        &self,
        kind: Kind,
        id: ObjectId,
        surface: ObjectId,
        region: ObjectId,
        lifetime: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(surface) = c.objects.surface(surface) else {
            c.invalid_object(surface);
            return Ok(());
        };
        let oneshot = match lifetime {
            LIFETIME_ONESHOT => true,
            LIFETIME_PERSISTENT => false,
            other => {
                c.protocol_error(self.id, 0, &format!("{other} is not a lifetime"));
                return Ok(());
            }
        };
        let Some(seat) = c.state.seat.borrow().clone() else {
            return Ok(());
        };
        if seat.constraint_for(&surface).is_some() {
            c.protocol_error(
                self.id,
                ERR_ALREADY_CONSTRAINED,
                "the surface already has a pointer constraint",
            );
            return Ok(());
        }
        let region = if region.0 == 0 {
            None
        } else {
            match c.objects.region(region) {
                Some(r) => Some(r.snapshot()),
                None => {
                    c.invalid_object(region);
                    return Ok(());
                }
            }
        };
        let con = Rc::new(Constraint {
            kind,
            oneshot,
            surface,
            obj: id,
            client: c.clone(),
            active: Cell::new(false),
            dead: Cell::new(false),
            hint: Cell::new(None),
            region: RefCell::new(region),
        });
        match kind {
            Kind::Lock => c.add_client_obj(Rc::new(LockedPointer {
                id,
                con: con.clone(),
            }))?,
            Kind::Confine => c.add_client_obj(Rc::new(ConfinedPointer {
                id,
                con: con.clone(),
            }))?,
        }
        seat.add_constraint(&c.state, con);
        Ok(())
    }
}

impl zwp_pointer_constraints_v1::Handler for ConstraintsManager {
    fn destroy(
        &self,
        _req: zwp_pointer_constraints_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn lock_pointer(
        &self,
        req: zwp_pointer_constraints_v1::lock_pointer::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.create(Kind::Lock, req.id, req.surface, req.region, req.lifetime)
    }

    fn confine_pointer(
        &self,
        req: zwp_pointer_constraints_v1::confine_pointer::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.create(Kind::Confine, req.id, req.surface, req.region, req.lifetime)
    }
}

impl Object for ConstraintsManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_pointer_constraints_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_pointer_constraints_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct LockedPointer {
    pub id: ObjectId,
    pub con: Rc<Constraint>,
}

impl zwp_locked_pointer_v1::Handler for LockedPointer {
    fn destroy(
        &self,
        _req: zwp_locked_pointer_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = self.con.client.clone();
        if let Some(seat) = c.state.seat.borrow().clone() {
            seat.remove_constraint(&c.state, &self.con);
        }
        c.remove_obj(self.id)?;
        Ok(())
    }

    fn set_cursor_position_hint(
        &self,
        req: zwp_locked_pointer_v1::set_cursor_position_hint::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.con
            .hint
            .set(Some((req.surface_x.to_f64(), req.surface_y.to_f64())));
        Ok(())
    }

    fn set_region(
        &self,
        req: zwp_locked_pointer_v1::set_region::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        set_region(&self.con, req.region)
    }
}

impl Object for LockedPointer {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_locked_pointer_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_locked_pointer_v1::dispatch(&*self, 1, opcode, r)
    }

    fn break_loops(&self) {
        let c = &self.con.client;
        if let Some(seat) = c.state.seat.borrow().clone() {
            seat.remove_constraint(&c.state, &self.con);
        }
    }
}

pub struct ConfinedPointer {
    pub id: ObjectId,
    pub con: Rc<Constraint>,
}

impl zwp_confined_pointer_v1::Handler for ConfinedPointer {
    fn destroy(
        &self,
        _req: zwp_confined_pointer_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = self.con.client.clone();
        if let Some(seat) = c.state.seat.borrow().clone() {
            seat.remove_constraint(&c.state, &self.con);
        }
        c.remove_obj(self.id)?;
        Ok(())
    }

    fn set_region(
        &self,
        req: zwp_confined_pointer_v1::set_region::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        set_region(&self.con, req.region)
    }
}

impl Object for ConfinedPointer {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwp_confined_pointer_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwp_confined_pointer_v1::dispatch(&*self, 1, opcode, r)
    }

    fn break_loops(&self) {
        let c = &self.con.client;
        if let Some(seat) = c.state.seat.borrow().clone() {
            seat.remove_constraint(&c.state, &self.con);
        }
    }
}

fn set_region(con: &Rc<Constraint>, region: ObjectId) -> Result<(), Box<dyn std::error::Error>> {
    let c = &con.client;
    *con.region.borrow_mut() = if region.0 == 0 {
        None
    } else {
        match c.objects.region(region) {
            Some(r) => Some(r.snapshot()),
            None => {
                c.invalid_object(region);
                return Ok(());
            }
        }
    };
    Ok(())
}
