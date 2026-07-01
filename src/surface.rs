// the surface tree. many surfaces per client, subsurfaces stack above and
// below their parent, input regions decide hit-testing to the deepest
// surface under the point.
//
// mapping is a pure function of the committed buffer: first commit with a
// buffer maps, removing the buffer unmaps. no mapped flag to desync.
//
// coordinates are i32 logical everywhere in the tree. scale is n/120 fixed
// point and only the renderer converts to physical pixels.

mod commit;
mod role;
