// xwayland lifecycle - spawn, handshake, then hand the wm socket to xwm.
//
// the child gets a fixed fd layout: 2 stderr, 3 displayfd, 4 x socket,
// 5 wm socket, 6 wayland socketpair (WAYLAND_SOCKET=6), launched with
// -terminate -rootless -displayfd 3 -listenfd 4 -wm 5. no Xauthority file
// is generated - the cookie rides in the connection handshake if one
// exists, otherwise auth is empty and the sockets are trusted.

mod xwm;
