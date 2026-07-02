// compositor-wide state - the single Rc root every subsystem hangs off.

use crate::engine::{Engine, Wheel};
use crate::uring::Ring;
use std::rc::Rc;

pub struct State {
    pub eng: Rc<Engine>,
    pub ring: Rc<Ring>,
    pub wheel: Wheel,
}

impl State {
    pub fn new(eng: &Rc<Engine>, ring: &Rc<Ring>, wheel: Wheel) -> Rc<State> {
        Rc::new(State {
            eng: eng.clone(),
            ring: ring.clone(),
            wheel,
        })
    }

    pub fn clear(&self) {
        self.wheel.clear();
    }
}
