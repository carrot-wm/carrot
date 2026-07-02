// xwayland lifecycle - spawn, handshake, then hand the wm socket to xwm.
//
// fixed child fd layout: 2 stderr, 3 displayfd, 4 x socket, 5 wm socket,
// 6 wayland socketpair (WAYLAND_SOCKET=6); launched -terminate -rootless
// -displayfd 3 -listenfd 4 -wm 5. no Xauthority file: the cookie rides in the
// connection handshake if one exists, else auth is empty and sockets trusted.

mod xwm;
