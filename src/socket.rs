use rustix::net::{AddressFamily, SocketFlags, SocketType, socket_with};
use rustix::fs::{open, flock, unlink, OFlags, Mode, FlockOperation};
use rustix::net::{SocketAddrUnix, bind};
use rustix::net::accept_with;
use std::os::fd::OwnedFd;
use rustix::net::listen;

pub struct WaylandSocket {
    pub name: String,
    pub path: String,
    pub fd: OwnedFd,
    pub lock_path: String,
    pub _lock_fd: OwnedFd,
}

impl WaylandSocket {
    pub fn new() -> Result<WaylandSocket, Box<dyn std::error::Error>> {

        let xrd = std::env::var("XDG_RUNTIME_DIR")?;

        // create the socket
        let fd = socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )?;

        let mut final_name = String::new();
        let mut final_path = String::new();
        let mut final_lock_path = String::new();
        let mut final_lock_fd: Option<OwnedFd> = None;

        // bind to a path
        for i in 1..1000 {
            let name = format!("wayland-{}", i);
            let path = format!("{}/{}", xrd, name);
            let addr = SocketAddrUnix::new(&path)?;

            let lock_path = format!("{}/{}.lock", xrd, name);
            let lock_fd = match open(
                &*lock_path,
                OFlags::CREATE | OFlags::RDWR | OFlags::CLOEXEC,
                Mode::from(0o644),
            )   {
                    Ok(fd) => fd,
                    Err(_) => continue,
                };

            if flock(&lock_fd, FlockOperation::NonBlockingLockExclusive).is_err() {
                continue;
            }

            let _ = unlink(&*path);

            // try to listen - if it fails, then try the next number
            if bind(&fd, &addr).is_err() {
                continue;
            }

            // worked - listen & break
            listen(&fd, 4096)?;
            final_name = name;
            final_path = path;
            final_lock_path = lock_path;
            final_lock_fd = Some(lock_fd);
            break;


        }

        if final_name.is_empty() {
            return Err("every wayland socket from 1 - 999 are already all in use".into());
        }
    
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &final_name); }
    Ok(WaylandSocket { name: final_name, path: final_path, fd, lock_path: final_lock_path, _lock_fd: final_lock_fd.expect("no socket was bound") })

    }

    pub fn accept(&self) -> Result<OwnedFd, Box<dyn std::error::Error>> {
        let client_fd = accept_with(&self.fd, SocketFlags::CLOEXEC)?;
        Ok(client_fd)
    }
}

impl Drop for WaylandSocket {
    fn drop(&mut self) {
        let _ = rustix::fs::unlink(&self.path);
        let _ = rustix::fs::unlink(&self.lock_path);
    }
}
