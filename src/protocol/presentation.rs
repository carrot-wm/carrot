// wp-presentation-time. feedback objects are one-shot and event-only:
// exactly one of presented/discarded fires, then the object dies. the
// timestamps come from the drm flip event, never from a clock read.

use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{wp_presentation, wp_presentation_feedback};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use std::cell::Cell;
use std::rc::Rc;

const CLOCK_MONOTONIC: u32 = 1;
pub const FLAG_VSYNC: u32 = 0x1;
pub const FLAG_HW_CLOCK: u32 = 0x2;
pub const FLAG_HW_COMPLETION: u32 = 0x4;

pub struct PresentationGlobal;

impl Global for PresentationGlobal {
    fn interface(&self) -> &'static str {
        wp_presentation::NAME
    }

    fn version(&self) -> u32 {
        2
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(WpPresentation {
            id,
            client: client.clone(),
            version,
        }))?;
        client.event(|o| wp_presentation::clock_id::send(o, id, CLOCK_MONOTONIC));
        Ok(())
    }
}

pub struct WpPresentation {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl wp_presentation::Handler for WpPresentation {
    fn destroy(
        &self,
        _req: wp_presentation::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn feedback(
        &self,
        req: wp_presentation::feedback::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(surface) = c.objects.surface(req.surface) else {
            c.invalid_object(req.surface);
            return Ok(());
        };
        c.add_client_obj(Rc::new(FeedbackObj { id: req.callback }))?;
        surface
            .pending
            .borrow_mut()
            .presentation_feedbacks
            .push(Feedback::new(c, req.callback));
        Ok(())
    }
}

impl Object for WpPresentation {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wp_presentation::NAME
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wp_presentation::dispatch(&*self, self.version, opcode, r)
    }
}

/// the client-facing id; requests never arrive (event-only interface)
struct FeedbackObj {
    id: ObjectId,
}

impl wp_presentation_feedback::Handler for FeedbackObj {}

impl Object for FeedbackObj {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wp_presentation_feedback::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wp_presentation_feedback::dispatch(&*self, 1, opcode, r)
    }
}

/// one requested feedback; fires exactly once, drop means discarded
pub struct Feedback {
    client: Rc<Client>,
    id: ObjectId,
    fired: Cell<bool>,
}

impl Feedback {
    fn new(client: &Rc<Client>, id: ObjectId) -> Feedback {
        Feedback {
            client: client.clone(),
            id,
            fired: Cell::new(false),
        }
    }

    pub fn presented(
        &self,
        output_name: &str,
        tv_sec: u32,
        tv_nsec: u32,
        refresh: u32,
        seq: u64,
        flags: u32,
    ) {
        if self.fired.replace(true) {
            return;
        }
        self.client.objects.for_each_output(|o| {
            if o.name == output_name {
                self.client
                    .event(|b| wp_presentation_feedback::sync_output::send(b, self.id, o.id));
            }
        });
        self.client.event(|b| {
            wp_presentation_feedback::presented::send(
                b,
                self.id,
                0,
                tv_sec,
                tv_nsec,
                refresh,
                (seq >> 32) as u32,
                seq as u32,
                flags,
            )
        });
        let _ = self.client.remove_obj(self.id);
    }

    pub fn discarded(&self) {
        if self.fired.replace(true) {
            return;
        }
        self.client
            .event(|b| wp_presentation_feedback::discarded::send(b, self.id));
        let _ = self.client.remove_obj(self.id);
    }
}

impl Drop for Feedback {
    fn drop(&mut self) {
        self.discarded();
    }
}
