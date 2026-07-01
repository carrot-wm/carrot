// the async engine. single threaded, everything is Rc, nothing is Send.
//
// each dispatch iteration drains four phases in order: EventHandling ->
// Layout -> PostLayout -> Present. client reads land in the first, client
// writes in PostLayout, so replies flush once per iteration after layout
// has settled.
//
// NOTE: never hold a RefCell borrow across an .await.

mod task;
mod wheel;
