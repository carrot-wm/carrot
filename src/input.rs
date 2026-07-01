// pure evdev + kbvm. fds come from logind TakeDevice, we never EVIOCGRAB.
//
// there is ONE set_focus path and everything that changes focus goes
// through it - scattered focus writes are how compositors segfault.
//
// keyboard, pointer and wheel come first; touchpad gestures, touchscreen
// and tablets after. nothing ships half done.

mod evdev;
mod focus;
mod seat;
