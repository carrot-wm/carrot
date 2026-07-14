// xdg-activation-v1: one app mints a token, hands it to another (env var
// or uri), and the holder asks for focus with it. tokens are single-use
// and short-lived; a valid activation focuses the window and follows it
// to its workspace, which is what makes "open link in running browser"
// actually land on the browser.

use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{xdg_activation_token_v1, xdg_activation_v1};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use std::cell::Cell;
use std::rc::Rc;

/// stale tokens age out; nothing legitimate waits half a minute to focus
const TOKEN_TTL_NS: u64 = 30_000_000_000;
const ERR_ALREADY_USED: u32 = 0;

pub struct ActivationGlobal;

impl Global for ActivationGlobal {
    fn interface(&self) -> &'static str {
        xdg_activation_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(Activation {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct Activation {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl xdg_activation_v1::Handler for Activation {
    fn destroy(
        &self,
        _req: xdg_activation_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn get_activation_token(
        &self,
        req: xdg_activation_v1::get_activation_token::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.add_client_obj(Rc::new(ActivationToken {
            id: req.id,
            client: self.client.clone(),
            version: self.version,
            committed: Cell::new(false),
        }))?;
        Ok(())
    }

    fn activate(
        &self,
        req: xdg_activation_v1::activate::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(surface) = c.objects.surface(req.surface) else {
            c.invalid_object(req.surface);
            return Ok(());
        };
        // single use: the lookup consumes the token; unknown or expired
        // ones are ignored - activation is a request, not a right
        let minted = c.state.activation_tokens.borrow_mut().remove(&req.token);
        let now = crate::util::Time::now().nsec();
        if minted.is_some_and(|t| now.saturating_sub(t) <= TOKEN_TTL_NS) {
            let root = surface.get_root();
            if let Some(win) = crate::tree::window_for_surface_any(&c.state, &root) {
                // the target may live on a hidden workspace: follow it
                if let Some(ws) = crate::tree::workspace_of(&c.state, &win) {
                    let idx = c.state.workspaces.borrow().iter().position(|w| Rc::ptr_eq(w, &ws));
                    if let Some(idx) = idx {
                        crate::tree::switch_workspace(&c.state, idx);
                    }
                }
                crate::tree::focus_window(&c.state, Some(&win));
                c.state.damage.trigger();
            }
        }
        Ok(())
    }
}

impl Object for Activation {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        xdg_activation_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        xdg_activation_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct ActivationToken {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    committed: Cell<bool>,
}

impl xdg_activation_token_v1::Handler for ActivationToken {
    // the serial/app-id/surface hints are accepted and unused: every
    // client that can reach the compositor may mint, and the token's
    // lifetime bounds abuse better than provenance checks would
    fn set_serial(
        &self,
        _req: xdg_activation_token_v1::set_serial::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn set_app_id(
        &self,
        _req: xdg_activation_token_v1::set_app_id::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn set_surface(
        &self,
        _req: xdg_activation_token_v1::set_surface::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn commit(
        &self,
        _req: xdg_activation_token_v1::commit::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        if self.committed.replace(true) {
            c.protocol_error(self.id, ERR_ALREADY_USED, "the token was already committed");
            return Ok(());
        }
        let token = format!("carrot-{:016x}", c.state.next_uid());
        let now = crate::util::Time::now().nsec();
        {
            // each mint sweeps: stale entries never outlive the next launch
            let mut live = c.state.activation_tokens.borrow_mut();
            live.retain(|_, t| now.saturating_sub(*t) <= TOKEN_TTL_NS);
            live.insert(token.clone(), now);
        }
        let id = self.id;
        c.event(|w| xdg_activation_token_v1::done::send(w, id, &token));
        Ok(())
    }

    fn destroy(
        &self,
        _req: xdg_activation_token_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for ActivationToken {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        xdg_activation_token_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        xdg_activation_token_v1::dispatch(&*self, self.version, opcode, r)
    }
}
