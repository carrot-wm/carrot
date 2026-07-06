// the wl_protocol! declarations. golden wire tests pin every opcode.

crate::wl_protocol! {
    interface wl_display, version = 1;
    request sync(callback: new_id);
    request get_registry(registry: new_id);
    event error(object_id: object, code: uint, message: string);
    event delete_id(id: uint);
}

crate::wl_protocol! {
    interface wl_registry, version = 1;
    request bind(name: uint, interface: string, version: uint, id: new_id);
    event global(name: uint, interface: string, version: uint);
    event global_remove(name: uint);
}

crate::wl_protocol! {
    interface wl_callback, version = 1;
    event done(callback_data: uint);
}

crate::wl_protocol! {
    interface wl_compositor, version = 6;
    request create_surface(id: new_id);
    request create_region(id: new_id);
}

crate::wl_protocol! {
    interface wl_surface, version = 6;
    request destroy();
    request attach(buffer: object, x: int, y: int);
    request damage(x: int, y: int, width: int, height: int);
    request frame(callback: new_id);
    request set_opaque_region(region: object);
    request set_input_region(region: object);
    request commit();
    request set_buffer_transform(transform: int) since 2;
    request set_buffer_scale(scale: int) since 3;
    request damage_buffer(x: int, y: int, width: int, height: int) since 4;
    request offset(x: int, y: int) since 5;
    event enter(output: object);
    event leave(output: object);
    event preferred_buffer_scale(factor: int) since 6;
    event preferred_buffer_transform(transform: uint) since 6;
}

crate::wl_protocol! {
    interface wl_region, version = 1;
    request destroy();
    request add(x: int, y: int, width: int, height: int);
    request subtract(x: int, y: int, width: int, height: int);
}

crate::wl_protocol! {
    interface wl_subcompositor, version = 1;
    request destroy();
    request get_subsurface(id: new_id, surface: object, parent: object);
}

crate::wl_protocol! {
    interface wl_subsurface, version = 1;
    request destroy();
    request set_position(x: int, y: int);
    request place_above(sibling: object);
    request place_below(sibling: object);
    request set_sync();
    request set_desync();
}

crate::wl_protocol! {
    interface wl_shm, version = 2;
    request create_pool(id: new_id, fd: fd, size: int);
    request release() since 2;
    event format(format: uint);
}

crate::wl_protocol! {
    interface wl_shm_pool, version = 1;
    request create_buffer(id: new_id, offset: int, width: int, height: int, stride: int, format: uint);
    request destroy();
    request resize(size: int);
}

crate::wl_protocol! {
    interface wl_buffer, version = 1;
    request destroy();
    event release();
}

crate::wl_protocol! {
    interface wl_seat, version = 9;
    request get_pointer(id: new_id);
    request get_keyboard(id: new_id);
    request get_touch(id: new_id);
    request release() since 5;
    event capabilities(capabilities: uint);
    event name(name: string) since 2;
}

crate::wl_protocol! {
    interface wl_pointer, version = 9;
    request set_cursor(serial: uint, surface: object, hotspot_x: int, hotspot_y: int);
    request release() since 3;
    event enter(serial: uint, surface: object, surface_x: fixed, surface_y: fixed);
    event leave(serial: uint, surface: object);
    event motion(time: uint, surface_x: fixed, surface_y: fixed);
    event button(serial: uint, time: uint, button: uint, state: uint);
    event axis(time: uint, axis: uint, value: fixed);
    event frame() since 5;
    event axis_source(axis_source: uint) since 5;
    event axis_stop(time: uint, axis: uint) since 5;
    event axis_discrete(axis: uint, discrete: int) since 5;
    event axis_value120(axis: uint, value120: int) since 8;
    event axis_relative_direction(axis: uint, direction: uint) since 9;
}

crate::wl_protocol! {
    interface wl_keyboard, version = 9;
    request release() since 3;
    event keymap(format: uint, fd: fd, size: uint);
    event enter(serial: uint, surface: object, keys: array);
    event leave(serial: uint, surface: object);
    event key(serial: uint, time: uint, key: uint, state: uint);
    event modifiers(serial: uint, mods_depressed: uint, mods_latched: uint, mods_locked: uint, group: uint);
    event repeat_info(rate: int, delay: int) since 4;
}

crate::wl_protocol! {
    interface xdg_wm_base, version = 6;
    request destroy();
    request create_positioner(id: new_id);
    request get_xdg_surface(id: new_id, surface: object);
    request pong(serial: uint);
    event ping(serial: uint);
}

crate::wl_protocol! {
    interface xdg_positioner, version = 6;
    request destroy();
    request set_size(width: int, height: int);
    request set_anchor_rect(x: int, y: int, width: int, height: int);
    request set_anchor(anchor: uint);
    request set_gravity(gravity: uint);
    request set_constraint_adjustment(constraint_adjustment: uint);
    request set_offset(x: int, y: int);
    request set_reactive() since 3;
    request set_parent_size(parent_width: int, parent_height: int) since 3;
    request set_parent_configure(serial: uint) since 3;
}

crate::wl_protocol! {
    interface xdg_surface, version = 6;
    request destroy();
    request get_toplevel(id: new_id);
    request get_popup(id: new_id, parent: object, positioner: object);
    request set_window_geometry(x: int, y: int, width: int, height: int);
    request ack_configure(serial: uint);
    event configure(serial: uint);
}

crate::wl_protocol! {
    interface xdg_toplevel, version = 6;
    request destroy();
    request set_parent(parent: object);
    request set_title(title: string);
    request set_app_id(app_id: string);
    request show_window_menu(seat: object, serial: uint, x: int, y: int);
    request r#move(seat: object, serial: uint);
    request resize(seat: object, serial: uint, edges: uint);
    request set_max_size(width: int, height: int);
    request set_min_size(width: int, height: int);
    request set_maximized();
    request unset_maximized();
    request set_fullscreen(output: object);
    request unset_fullscreen();
    request set_minimized();
    event configure(width: int, height: int, states: array);
    event close();
    event configure_bounds(width: int, height: int) since 4;
    event wm_capabilities(capabilities: array) since 5;
}

crate::wl_protocol! {
    interface xdg_popup, version = 6;
    request destroy();
    request grab(seat: object, serial: uint);
    request reposition(positioner: object, token: uint) since 3;
    event configure(x: int, y: int, width: int, height: int);
    event popup_done();
    event repositioned(token: uint) since 3;
}

crate::wl_protocol! {
    interface wl_data_device_manager, version = 3;
    request create_data_source(id: new_id);
    request get_data_device(id: new_id, seat: object);
}

crate::wl_protocol! {
    interface wl_data_source, version = 3;
    request offer(mime_type: string);
    request destroy();
    request set_actions(dnd_actions: uint) since 3;
    event target(mime_type: optstring);
    event send(mime_type: string, fd: fd);
    event cancelled();
    event dnd_drop_performed() since 3;
    event dnd_finished() since 3;
    event action(dnd_action: uint) since 3;
}

crate::wl_protocol! {
    interface wl_data_device, version = 3;
    request start_drag(source: object, origin: object, icon: object, serial: uint);
    request set_selection(source: object, serial: uint);
    request release() since 2;
    event data_offer(id: object);
    event enter(serial: uint, surface: object, x: fixed, y: fixed, id: object);
    event leave();
    event motion(time: uint, x: fixed, y: fixed);
    event drop();
    event selection(id: object);
}

crate::wl_protocol! {
    interface wl_data_offer, version = 3;
    request accept(serial: uint, mime_type: optstring);
    request receive(mime_type: string, fd: fd);
    request destroy();
    request finish() since 3;
    request set_actions(dnd_actions: uint, preferred_action: uint) since 3;
    event offer(mime_type: string);
    event source_actions(source_actions: uint) since 3;
    event action(dnd_action: uint) since 3;
}

crate::wl_protocol! {
    interface zxdg_decoration_manager_v1, version = 1;
    request destroy();
    request get_toplevel_decoration(id: new_id, toplevel: object);
}

crate::wl_protocol! {
    interface zxdg_toplevel_decoration_v1, version = 1;
    request destroy();
    request set_mode(mode: uint);
    request unset_mode();
    event configure(mode: uint);
}

crate::wl_protocol! {
    interface zwp_primary_selection_device_manager_v1, version = 1;
    request create_source(id: new_id);
    request get_device(id: new_id, seat: object);
    request destroy();
}

crate::wl_protocol! {
    interface zwp_primary_selection_source_v1, version = 1;
    request offer(mime_type: string);
    request destroy();
    event send(mime_type: string, fd: fd);
    event cancelled();
}

crate::wl_protocol! {
    interface zwp_primary_selection_device_v1, version = 1;
    request set_selection(source: object, serial: uint);
    request destroy();
    event data_offer(offer: object);
    event selection(id: object);
}

crate::wl_protocol! {
    interface zwp_primary_selection_offer_v1, version = 1;
    request receive(mime_type: string, fd: fd);
    request destroy();
    event offer(mime_type: string);
}

crate::wl_protocol! {
    interface zwlr_layer_shell_v1, version = 5;
    request get_layer_surface(id: new_id, surface: object, output: object, layer: uint, namespace: string);
    request destroy() since 3;
}

crate::wl_protocol! {
    interface zwlr_layer_surface_v1, version = 5;
    request set_size(width: uint, height: uint);
    request set_anchor(anchor: uint);
    request set_exclusive_zone(zone: int);
    request set_margin(top: int, right: int, bottom: int, left: int);
    request set_keyboard_interactivity(keyboard_interactivity: uint);
    request get_popup(popup: object);
    request ack_configure(serial: uint);
    request destroy();
    request set_layer(layer: uint) since 2;
    request set_exclusive_edge(edge: uint) since 5;
    event configure(serial: uint, width: uint, height: uint);
    event closed();
}

crate::wl_protocol! {
    interface zxdg_output_manager_v1, version = 3;
    request destroy();
    request get_xdg_output(id: new_id, output: object);
}

crate::wl_protocol! {
    interface zxdg_output_v1, version = 3;
    request destroy();
    event logical_position(x: int, y: int);
    event logical_size(width: int, height: int);
    event done();
    event name(name: string) since 2;
    event description(description: string) since 2;
}

crate::wl_protocol! {
    interface wl_output, version = 4;
    request release() since 3;
    event geometry(x: int, y: int, physical_width: int, physical_height: int, subpixel: int, make: string, model: string, transform: int);
    event mode(flags: uint, width: int, height: int, refresh: int);
    event done() since 2;
    event scale(factor: int) since 2;
    event name(name: string) since 4;
    event description(description: string) since 4;
}

// wl_display.error codes
pub const INVALID_OBJECT: u32 = 0;
pub const INVALID_METHOD: u32 = 1;
#[allow(dead_code)]
pub const NO_MEMORY: u32 = 2;
pub const IMPLEMENTATION: u32 = 3;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::wire::{EventOut, MsgReader, WireError};
    use crate::protocol::{DispatchError, Fixed, ObjectId};
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    #[test]
    fn opcodes_are_pinned() {
        assert_eq!(wl_display::sync::OPCODE, 0);
        assert_eq!(wl_display::get_registry::OPCODE, 1);
        assert_eq!(wl_display::error::OPCODE, 0);
        assert_eq!(wl_display::delete_id::OPCODE, 1);
        assert_eq!(wl_registry::bind::OPCODE, 0);
        assert_eq!(wl_registry::global::OPCODE, 0);
        assert_eq!(wl_registry::global_remove::OPCODE, 1);
        assert_eq!(wl_callback::done::OPCODE, 0);
        assert_eq!(wl_compositor::create_surface::OPCODE, 0);
        assert_eq!(wl_compositor::create_region::OPCODE, 1);
        assert_eq!(wl_surface::destroy::OPCODE, 0);
        assert_eq!(wl_surface::attach::OPCODE, 1);
        assert_eq!(wl_surface::damage::OPCODE, 2);
        assert_eq!(wl_surface::frame::OPCODE, 3);
        assert_eq!(wl_surface::set_opaque_region::OPCODE, 4);
        assert_eq!(wl_surface::set_input_region::OPCODE, 5);
        assert_eq!(wl_surface::commit::OPCODE, 6);
        assert_eq!(wl_surface::set_buffer_transform::OPCODE, 7);
        assert_eq!(wl_surface::set_buffer_scale::OPCODE, 8);
        assert_eq!(wl_surface::damage_buffer::OPCODE, 9);
        assert_eq!(wl_surface::offset::OPCODE, 10);
        assert_eq!(wl_surface::enter::OPCODE, 0);
        assert_eq!(wl_surface::leave::OPCODE, 1);
        assert_eq!(wl_surface::preferred_buffer_scale::OPCODE, 2);
        assert_eq!(wl_surface::preferred_buffer_transform::OPCODE, 3);
        assert_eq!(wl_region::destroy::OPCODE, 0);
        assert_eq!(wl_region::add::OPCODE, 1);
        assert_eq!(wl_region::subtract::OPCODE, 2);
        assert_eq!(wl_subcompositor::get_subsurface::OPCODE, 1);
        assert_eq!(wl_subsurface::destroy::OPCODE, 0);
        assert_eq!(wl_subsurface::set_position::OPCODE, 1);
        assert_eq!(wl_subsurface::place_above::OPCODE, 2);
        assert_eq!(wl_subsurface::place_below::OPCODE, 3);
        assert_eq!(wl_subsurface::set_sync::OPCODE, 4);
        assert_eq!(wl_subsurface::set_desync::OPCODE, 5);
        assert_eq!(wl_shm::create_pool::OPCODE, 0);
        assert_eq!(wl_shm::release::OPCODE, 1);
        assert_eq!(wl_shm::format::OPCODE, 0);
        assert_eq!(wl_shm_pool::create_buffer::OPCODE, 0);
        assert_eq!(wl_shm_pool::destroy::OPCODE, 1);
        assert_eq!(wl_shm_pool::resize::OPCODE, 2);
        assert_eq!(wl_buffer::destroy::OPCODE, 0);
        assert_eq!(wl_buffer::release::OPCODE, 0);
        assert_eq!(wl_seat::release::OPCODE, 3);
        assert_eq!(wl_seat::name::OPCODE, 1);
        assert_eq!(wl_seat::name::SINCE, 2);
        assert_eq!(wl_pointer::release::OPCODE, 1);
        assert_eq!(wl_pointer::frame::OPCODE, 5);
        assert_eq!(wl_pointer::axis_value120::OPCODE, 9);
        assert_eq!(wl_pointer::axis_value120::SINCE, 8);
        assert_eq!(wl_pointer::axis_relative_direction::OPCODE, 10);
        assert_eq!(wl_keyboard::keymap::OPCODE, 0);
        assert_eq!(wl_keyboard::modifiers::OPCODE, 4);
        assert_eq!(wl_keyboard::repeat_info::OPCODE, 5);
        assert_eq!(wl_keyboard::repeat_info::SINCE, 4);
        assert_eq!(xdg_wm_base::destroy::OPCODE, 0);
        assert_eq!(xdg_wm_base::create_positioner::OPCODE, 1);
        assert_eq!(xdg_wm_base::get_xdg_surface::OPCODE, 2);
        assert_eq!(xdg_wm_base::pong::OPCODE, 3);
        assert_eq!(xdg_wm_base::ping::OPCODE, 0);
        assert_eq!(xdg_positioner::set_size::OPCODE, 1);
        assert_eq!(xdg_positioner::set_offset::OPCODE, 6);
        assert_eq!(xdg_positioner::set_parent_configure::OPCODE, 9);
        assert_eq!(xdg_surface::get_toplevel::OPCODE, 1);
        assert_eq!(xdg_surface::get_popup::OPCODE, 2);
        assert_eq!(xdg_surface::set_window_geometry::OPCODE, 3);
        assert_eq!(xdg_surface::ack_configure::OPCODE, 4);
        assert_eq!(xdg_surface::configure::OPCODE, 0);
        assert_eq!(xdg_toplevel::set_parent::OPCODE, 1);
        assert_eq!(xdg_toplevel::r#move::OPCODE, 5);
        assert_eq!(xdg_toplevel::set_max_size::OPCODE, 7);
        assert_eq!(xdg_toplevel::set_fullscreen::OPCODE, 11);
        assert_eq!(xdg_toplevel::set_minimized::OPCODE, 13);
        assert_eq!(xdg_toplevel::configure::OPCODE, 0);
        assert_eq!(xdg_toplevel::close::OPCODE, 1);
        assert_eq!(xdg_toplevel::wm_capabilities::OPCODE, 3);
        assert_eq!(xdg_toplevel::wm_capabilities::SINCE, 5);
        assert_eq!(xdg_popup::grab::OPCODE, 1);
        assert_eq!(xdg_popup::reposition::OPCODE, 2);
        assert_eq!(xdg_popup::configure::OPCODE, 0);
        assert_eq!(xdg_popup::popup_done::OPCODE, 1);
        assert_eq!(wl_data_device_manager::create_data_source::OPCODE, 0);
        assert_eq!(wl_data_device_manager::get_data_device::OPCODE, 1);
        assert_eq!(wl_data_source::offer::OPCODE, 0);
        assert_eq!(wl_data_source::set_actions::OPCODE, 2);
        assert_eq!(wl_data_source::target::OPCODE, 0);
        assert_eq!(wl_data_source::send::OPCODE, 1);
        assert_eq!(wl_data_source::cancelled::OPCODE, 2);
        assert_eq!(wl_data_device::start_drag::OPCODE, 0);
        assert_eq!(wl_data_device::set_selection::OPCODE, 1);
        assert_eq!(wl_data_device::release::OPCODE, 2);
        assert_eq!(wl_data_device::data_offer::OPCODE, 0);
        assert_eq!(wl_data_device::selection::OPCODE, 5);
        assert_eq!(wl_data_offer::accept::OPCODE, 0);
        assert_eq!(wl_data_offer::receive::OPCODE, 1);
        assert_eq!(wl_data_offer::finish::OPCODE, 3);
        assert_eq!(wl_data_offer::offer::OPCODE, 0);
        assert_eq!(zxdg_decoration_manager_v1::get_toplevel_decoration::OPCODE, 1);
        assert_eq!(zxdg_toplevel_decoration_v1::set_mode::OPCODE, 1);
        assert_eq!(zxdg_toplevel_decoration_v1::unset_mode::OPCODE, 2);
        assert_eq!(zxdg_toplevel_decoration_v1::configure::OPCODE, 0);
        assert_eq!(zwp_primary_selection_device_manager_v1::create_source::OPCODE, 0);
        assert_eq!(zwp_primary_selection_device_manager_v1::get_device::OPCODE, 1);
        assert_eq!(zwp_primary_selection_source_v1::offer::OPCODE, 0);
        assert_eq!(zwp_primary_selection_source_v1::send::OPCODE, 0);
        assert_eq!(zwp_primary_selection_source_v1::cancelled::OPCODE, 1);
        assert_eq!(zwp_primary_selection_device_v1::set_selection::OPCODE, 0);
        assert_eq!(zwp_primary_selection_device_v1::data_offer::OPCODE, 0);
        assert_eq!(zwp_primary_selection_device_v1::selection::OPCODE, 1);
        assert_eq!(zwp_primary_selection_offer_v1::receive::OPCODE, 0);
        assert_eq!(zwp_primary_selection_offer_v1::offer::OPCODE, 0);
        assert_eq!(zwlr_layer_shell_v1::get_layer_surface::OPCODE, 0);
        assert_eq!(zwlr_layer_shell_v1::destroy::OPCODE, 1);
        assert_eq!(zwlr_layer_shell_v1::destroy::SINCE, 3);
        assert_eq!(zwlr_layer_surface_v1::set_size::OPCODE, 0);
        assert_eq!(zwlr_layer_surface_v1::set_anchor::OPCODE, 1);
        assert_eq!(zwlr_layer_surface_v1::set_exclusive_zone::OPCODE, 2);
        assert_eq!(zwlr_layer_surface_v1::set_margin::OPCODE, 3);
        assert_eq!(zwlr_layer_surface_v1::set_keyboard_interactivity::OPCODE, 4);
        assert_eq!(zwlr_layer_surface_v1::get_popup::OPCODE, 5);
        assert_eq!(zwlr_layer_surface_v1::ack_configure::OPCODE, 6);
        assert_eq!(zwlr_layer_surface_v1::destroy::OPCODE, 7);
        assert_eq!(zwlr_layer_surface_v1::set_layer::OPCODE, 8);
        assert_eq!(zwlr_layer_surface_v1::set_layer::SINCE, 2);
        assert_eq!(zwlr_layer_surface_v1::set_exclusive_edge::OPCODE, 9);
        assert_eq!(zwlr_layer_surface_v1::set_exclusive_edge::SINCE, 5);
        assert_eq!(zwlr_layer_surface_v1::configure::OPCODE, 0);
        assert_eq!(zwlr_layer_surface_v1::closed::OPCODE, 1);
        assert_eq!(zxdg_output_manager_v1::get_xdg_output::OPCODE, 1);
        assert_eq!(zxdg_output_v1::logical_position::OPCODE, 0);
        assert_eq!(zxdg_output_v1::logical_size::OPCODE, 1);
        assert_eq!(zxdg_output_v1::done::OPCODE, 2);
        assert_eq!(zxdg_output_v1::name::OPCODE, 3);
        assert_eq!(zxdg_output_v1::name::SINCE, 2);
        assert_eq!(wl_output::release::OPCODE, 0);
        assert_eq!(wl_output::geometry::OPCODE, 0);
        assert_eq!(wl_output::mode::OPCODE, 1);
        assert_eq!(wl_output::done::OPCODE, 2);
        assert_eq!(wl_output::scale::OPCODE, 3);
        assert_eq!(wl_output::name::OPCODE, 4);
        assert_eq!(wl_output::description::OPCODE, 5);
    }

    /// byte-exact fixtures pin arg offsets and padding, not just opcodes
    #[test]
    fn golden_registry_global() {
        let mut o = EventOut::default();
        wl_registry::global::send(&mut o, ObjectId(2), 3, "wl_compositor", 6);
        let mut want = Vec::new();
        want.extend(2u32.to_ne_bytes());
        // len 36 = hdr 8 + name 4 + (strlen 4 + "wl_compositor\0" padded 16) + version 4
        want.extend((36u32 << 16).to_ne_bytes());
        want.extend(3u32.to_ne_bytes());
        want.extend(14u32.to_ne_bytes());
        want.extend(b"wl_compositor\0\0\0");
        want.extend(6u32.to_ne_bytes());
        assert_eq!(o.bytes, want);
        assert!(o.fds.is_empty());
    }

    #[test]
    fn golden_display_error() {
        let mut o = EventOut::default();
        wl_display::error::send(&mut o, ObjectId(1), ObjectId(9), 3, "oops");
        let mut want = Vec::new();
        want.extend(1u32.to_ne_bytes());
        // len 28 = hdr 8 + object 4 + code 4 + (strlen 4 + "oops\0" padded 8)
        want.extend((28u32 << 16).to_ne_bytes());
        want.extend(9u32.to_ne_bytes());
        want.extend(3u32.to_ne_bytes());
        want.extend(5u32.to_ne_bytes());
        want.extend(b"oops\0\0\0\0");
        assert_eq!(o.bytes, want);
    }

    #[test]
    fn golden_callback_done() {
        let mut o = EventOut::default();
        wl_callback::done::send(&mut o, ObjectId(5), 0);
        let mut want = Vec::new();
        want.extend(5u32.to_ne_bytes());
        want.extend((12u32 << 16).to_ne_bytes());
        want.extend(0u32.to_ne_bytes());
        assert_eq!(o.bytes, want);
    }

    fn body_of(bytes: &[u8]) -> &[u8] {
        &bytes[8..]
    }

    #[test]
    fn bind_request_parses() {
        let mut o = EventOut::default();
        {
            use crate::protocol::wire::MsgWriter;
            let mut w = MsgWriter::new(&mut o, ObjectId(2), 0);
            w.uint(3);
            w.string("wl_compositor");
            w.uint(6);
            w.object(ObjectId(4));
            w.finish();
        }
        let mut fds = VecDeque::new();
        let mut r = MsgReader::new(body_of(&o.bytes), &mut fds);
        let req = wl_registry::bind::Request::parse(&mut r).unwrap();
        assert_eq!(req.name, 3);
        assert_eq!(req.interface, "wl_compositor");
        assert_eq!(req.version, 6);
        assert_eq!(req.id, ObjectId(4));
    }

    #[test]
    fn trailing_data_rejected() {
        let body = 7u32.to_ne_bytes().repeat(2);
        let mut fds = VecDeque::new();
        let mut r = MsgReader::new(&body, &mut fds);
        let err = wl_display::sync::Request::parse(&mut r).unwrap_err();
        assert_eq!(err, WireError::TrailingData);
    }

    #[test]
    fn truncated_body_rejected() {
        let mut fds = VecDeque::new();
        let mut r = MsgReader::new(&[], &mut fds);
        let err = wl_display::sync::Request::parse(&mut r).unwrap_err();
        assert_eq!(err, WireError::Truncated);
    }

    #[test]
    fn empty_required_string_rejected() {
        // bind with a zero-length interface string
        let mut body = Vec::new();
        body.extend(3u32.to_ne_bytes());
        body.extend(0u32.to_ne_bytes());
        body.extend(6u32.to_ne_bytes());
        body.extend(4u32.to_ne_bytes());
        let mut fds = VecDeque::new();
        let mut r = MsgReader::new(&body, &mut fds);
        let err = wl_registry::bind::Request::parse(&mut r).unwrap_err();
        assert_eq!(err, WireError::BadString);
    }

    // a test-only interface covering every wire type plus since-gating
    crate::wl_protocol! {
        interface test_iface, version = 3;
        request all_types(a: int, b: uint, c: fixed, d: object, e: string, f: optstring, g: array, h: fd);
        request newer(x: uint) since 3;
        event echo(a: int, c: fixed, f: optstring, g: array);
    }

    struct TestHandler {
        newest: Cell<u32>,
    }

    impl test_iface::Handler for TestHandler {
        fn all_types(
            &self,
            req: test_iface::all_types::Request,
        ) -> Result<(), Box<dyn std::error::Error>> {
            assert_eq!(req.a, -5);
            assert_eq!(req.b, 7);
            assert_eq!(req.c, Fixed::from_int(2));
            assert_eq!(req.d, ObjectId(11));
            assert_eq!(req.e, "hi");
            assert_eq!(req.f, None);
            assert_eq!(req.g, [1, 2, 3]);
            Ok(())
        }

        fn newer(&self, req: test_iface::newer::Request) -> Result<(), Box<dyn std::error::Error>> {
            self.newest.set(req.x);
            Ok(())
        }
    }

    #[test]
    fn all_types_roundtrip_through_dispatch() {
        let mut o = EventOut::default();
        {
            use crate::protocol::wire::MsgWriter;
            let mut w = MsgWriter::new(&mut o, ObjectId(3), 0);
            w.int(-5);
            w.uint(7);
            w.fixed(Fixed::from_int(2));
            w.object(ObjectId(11));
            w.string("hi");
            w.optstring(None);
            w.array(&[1, 2, 3]);
            w.finish();
        }
        let efd = rustix::event::eventfd(0, rustix::event::EventfdFlags::empty()).unwrap();
        let mut fds = VecDeque::from([efd]);
        let mut r = MsgReader::new(body_of(&o.bytes), &mut fds);
        let h = TestHandler { newest: Cell::new(0) };
        test_iface::dispatch(&h, 3, 0, &mut r).unwrap();
    }

    #[test]
    fn since_gates_dispatch_by_bound_version() {
        let h = TestHandler { newest: Cell::new(0) };
        let body = 9u32.to_ne_bytes();

        let mut fds = VecDeque::new();
        let mut r = MsgReader::new(&body, &mut fds);
        let err = test_iface::dispatch(&h, 2, test_iface::newer::OPCODE, &mut r).unwrap_err();
        assert!(matches!(err, DispatchError::UnknownOpcode(_)));
        assert_eq!(h.newest.get(), 0);

        let mut fds = VecDeque::new();
        let mut r = MsgReader::new(&body, &mut fds);
        test_iface::dispatch(&h, 3, test_iface::newer::OPCODE, &mut r).unwrap();
        assert_eq!(h.newest.get(), 9);
    }

    #[test]
    fn event_fds_ride_the_side_channel() {
        let efd = rustix::event::eventfd(0, rustix::event::EventfdFlags::empty()).unwrap();
        let mut o = EventOut::default();
        {
            use crate::protocol::wire::MsgWriter;
            let mut w = MsgWriter::new(&mut o, ObjectId(1), 0);
            w.fd(Rc::new(efd));
            w.uint(1);
            w.finish();
        }
        // fds never contribute wire bytes
        assert_eq!(o.bytes.len(), 12);
        assert_eq!(o.fds.len(), 1);
    }

    #[test]
    fn missing_fd_is_an_error() {
        let mut o = EventOut::default();
        {
            use crate::protocol::wire::MsgWriter;
            let mut w = MsgWriter::new(&mut o, ObjectId(3), 0);
            w.int(-5);
            w.uint(7);
            w.fixed(Fixed::from_int(2));
            w.object(ObjectId(11));
            w.string("hi");
            w.optstring(None);
            w.array(&[1, 2, 3]);
            w.finish();
        }
        let mut fds = VecDeque::new();
        let mut r = MsgReader::new(body_of(&o.bytes), &mut fds);
        let err = test_iface::all_types::Request::parse(&mut r).unwrap_err();
        assert_eq!(err, WireError::MissingFd);
    }
}
