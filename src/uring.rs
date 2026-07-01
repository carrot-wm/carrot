// io_uring, proactor style - ops go in owning their buffers and come back
// out with results, nothing polls and then reads.
//
// ring setup: SINGLE_ISSUER | DEFER_TASKRUN | COOP_TASKRUN | SUBMIT_ALL,
// and NODROP is non-negotiable. a full sq is a hard error, not something
// to silently drop. cqes drain in batches.

mod ops;
