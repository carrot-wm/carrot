// atomic kms. per-connector state from day one, double-buffered scanout,
// hardware cursor on its own plane, cursor flushes independent of flips.
//
// EBUSY means rebuild the whole commit (fence and cursor included) and try
// again - never a degraded partial. OUT_FENCE_PTR goes on every commit,
// even right after one fails. hotplug is a raw netlink uevent socket, no
// libudev.

mod atomic;
mod connector;
mod uevent;
