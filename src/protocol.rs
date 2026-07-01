// the wayland protocol layer. wl_protocol! is the single source of truth -
// interfaces, opcodes, stubs all generate from one declaration, and requests
// and events are numbered separately. wl_array and fd args are first class,
// nothing on the wire is ever hand-packed.

mod interfaces;
mod wire;
