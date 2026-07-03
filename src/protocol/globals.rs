// the global registry: what wl_registry advertises and binds. names are
// never reused.

use crate::client::{Client, ClientError};
use crate::protocol::ObjectId;
use crate::protocol::display::WlRegistry;
use crate::util::NumCell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};

pub trait Global {
    fn interface(&self) -> &'static str;
    /// highest version we advertise; a bind gets what it asks for up to this
    fn version(&self) -> u32;
    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError>;
}

pub struct Globals {
    next_name: NumCell<u32>,
    /// BTreeMap so advertisement order is stable
    map: RefCell<BTreeMap<u32, Rc<dyn Global>>>,
    /// every live wl_registry; runtime add/remove broadcasts land here
    registries: RefCell<Vec<Weak<WlRegistry>>>,
}

impl Default for Globals {
    fn default() -> Self {
        Globals {
            next_name: NumCell::new(1),
            map: RefCell::new(BTreeMap::new()),
            registries: RefCell::new(Vec::new()),
        }
    }
}

impl Globals {
    pub fn add(&self, g: Rc<dyn Global>) -> u32 {
        let name = self.next_name.fetch_add(1);
        self.map.borrow_mut().insert(name, g.clone());
        self.broadcast(|reg| reg.send_global(name, g.interface(), g.version()));
        name
    }

    pub fn remove(&self, name: u32) {
        if self.map.borrow_mut().remove(&name).is_some() {
            self.broadcast(|reg| reg.send_global_remove(name));
        }
    }

    pub fn get(&self, name: u32) -> Option<Rc<dyn Global>> {
        self.map.borrow().get(&name).cloned()
    }

    /// announce everything to a fresh registry, then keep it for later add/remove
    pub fn subscribe(&self, registry: &Rc<WlRegistry>) {
        for (name, g) in self.map.borrow().iter() {
            registry.send_global(*name, g.interface(), g.version());
        }
        self.registries.borrow_mut().push(Rc::downgrade(registry));
    }

    fn broadcast(&self, f: impl Fn(&WlRegistry)) {
        self.registries.borrow_mut().retain(|w| match w.upgrade() {
            Some(reg) => {
                f(&reg);
                true
            }
            None => false,
        });
    }
}
