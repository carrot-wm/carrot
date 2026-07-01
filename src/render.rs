// renderer trait + the vulkan/ash implementation.
//
// a frame is an ordered pass list. v1 has exactly one pass (composite the
// textured quads), blur and animation passes slot in later without a
// rewrite. push descriptors are required at device creation. damage decides
// what renders, vblank decides when callbacks fire.
//
// NOTE: premult alpha, dual image views, per-window scissor, geometry
// offset - the four things that silently break real clients when missing.

mod vulkan;
