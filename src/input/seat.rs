// the seat - each client can bind wl_seat multiple times at different
// versions, so events gate on the binding's version, not the client's.
// a late get_pointer/get_keyboard gets an immediate enter if we hold focus.
