// keyboard focus. order: leave old, enter new with the held-keys array,
// then modifiers - skip any step and clients desync.

use super::seat::SeatGlobal;
use crate::protocol::interfaces::wl_keyboard;
use crate::state::State;
use crate::surface::WlSurface;
use std::rc::Rc;

pub fn set_keyboard_focus(state: &Rc<State>, seat: &Rc<SeatGlobal>, new: Option<Rc<WlSurface>>) {
    // a destroyed surface's id may already be recycled client-side;
    // naming it in enter/leave is a fatal error over there
    let new = new.filter(|s| !s.destroyed.get());
    // while locked the keyboard belongs to lock surfaces; activation,
    // layer locks and window focus all wait for the unlock
    if crate::protocol::session_lock::locked(state) {
        let allowed = new
            .as_ref()
            .is_none_or(|s| s.get_root().role.get() == crate::surface::SurfaceRole::LockSurface);
        if !allowed {
            return;
        }
    }
    let old = seat.kb_focus.borrow().clone();
    match (&old, &new) {
        (Some(a), Some(b)) if Rc::ptr_eq(a, b) => return,
        (None, None) => return,
        _ => {}
    }
    // a repeat must never leak across surfaces
    seat.cancel_repeat();
    if let Some(old) = &old {
        if !old.destroyed.get() {
            let serial = state.next_serial(Some(&old.client)) as u32;
            seat.for_each_keyboard(old.client.id, 1, |kb| {
                kb.client
                    .event(|o| wl_keyboard::leave::send(o, kb.id, serial, old.id));
            });
        }
        old.ext.borrow().set_active(false);
    }
    let old_win = old.as_ref().and_then(|s| crate::tree::window_for_surface(state, s));
    *seat.kb_focus.borrow_mut() = new.clone();
    let new_win = new.as_ref().and_then(|s| crate::tree::window_for_surface(state, s));
    // when focus leaves the x world entirely, the x server must be told:
    // a hidden client that keeps core focus reads keys that now belong
    // to a wl window. focusing another x window replaces focus instead
    let old_x = old_win.as_ref().and_then(|w| w.x11_opt()).cloned();
    let new_is_x = new_win.as_ref().is_some_and(|w| w.x11_opt().is_some());
    if let Some(xw) = old_x
        && !new_is_x
    {
        xw.drop_focus();
    }
    crate::protocol::foreign_toplevel::focus_changed(state, old_win, new_win);
    if let Some(new) = &new {
        let serial = state.next_serial(Some(&new.client)) as u32;
        let keys = seat.keys_bytes();
        let mods = seat.mods.get();
        seat.for_each_keyboard(new.client.id, 1, |kb| {
            kb.client
                .event(|o| wl_keyboard::enter::send(o, kb.id, serial, new.id, &keys));
            kb.send_modifiers(serial, mods);
        });
        new.ext.borrow().set_active(true);
        // the keyboard owner learns what both clipboards hold
        seat.data.offer_to(&new.client);
        seat.primary.offer_to(&new.client);
        if let Some(win) = crate::tree::window_for_surface(state, new) {
            // x clients don't get focus from wl_keyboard::enter alone
            if let Some(xw) = win.x11_opt() {
                xw.take_focus();
            }
            crate::ipc::emit(
                state,
                &serde_json::json!({ "window-focused": {
                    "title": win.title(),
                    "app-id": win.app_id(),
                }}),
            );
        }
    }
}
