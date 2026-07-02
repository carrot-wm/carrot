// our x11 wire library. async all the way down - a property read that can
// stall the compositor is a bug. request/reply/event definitions generate
// from spec files: nothing on the wire is hand-numbered.

mod auth;
mod wire;
