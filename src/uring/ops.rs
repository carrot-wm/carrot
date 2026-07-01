// one type per op - accept, read/write, recvmsg/sendmsg, timeout, poll,
// cancel. buffers stay owned by the op until its cqe lands, then the
// pending future wakes with them.
