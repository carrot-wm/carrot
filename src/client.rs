// per-client everything. one Rc<Client> per connection, and window keys are
// always (surface_id, client) - a surface id on its own means nothing.
//
// the object lifecycle is real: delete_id on destroy, ids get reclaimed,
// server-side ids start at 0xff000000. protocol errors kill the client
// loudly through wl_display.error, never a silent drop.

mod objects;
mod tasks;
