// hand-rolled dbus, just enough for logind.
//
// bring-up order matters: EXTERNAL auth + NEGOTIATE_UNIX_FD, Hello,
// GetSession (GetSessionByPID as the fallback), TakeControl, AddMatch,
// then TakeDevice per device. fds ride out of band - count them per
// message via UNIX_FDS or they get attributed to the wrong reply.
//
// TODO: endianness marker must be host endian, not a hardcoded 'l'

mod logind;
mod wire;
