// dwindle tree + workspaces + the floating stack.
//
// nodes have stable identity - no vec indices, ever. z-order is
// fullscreen > floats > tiled (float_above_fullscreen swaps the first
// two). new windows split whatever is under the cursor, not the focused
// window.

mod dwindle;
mod float;
mod workspace;
