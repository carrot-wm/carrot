// per-client buffer transport. rx is one linear accumulator with compaction;
// tx is a swapchain of out-buffers so event formatting never touches memory
// the kernel is reading. all buffers are owned Vecs that pass through ring ops.

use super::ClientError;
use crate::protocol::ObjectId;
use crate::protocol::wire::{EventOut, MAX_MESSAGE};
use crate::uring::{Ring, RingError};
use crate::util::Time;
use rustix::io::Errno;
use std::collections::VecDeque;
use std::mem;
use std::ops::Range;
use std::os::fd::OwnedFd;
use std::rc::Rc;

const RX_SIZE: usize = 2 * MAX_MESSAGE;
const OUT_FULL: usize = MAX_MESSAGE; // capacity is 2x, so a max message still fits
pub const MAX_RX_FDS: usize = 32;
pub const LIMIT_PENDING: usize = 10;

// -- receiving --

pub struct RxMessage {
    pub object: ObjectId,
    pub opcode: u32,
    pub body: Range<usize>,
}

pub struct RxBuffer {
    buf: Vec<u8>,
    lo: usize,
    len: usize,
    fds: VecDeque<OwnedFd>,
}

impl RxBuffer {
    pub fn new() -> RxBuffer {
        RxBuffer {
            buf: vec![0; RX_SIZE],
            lo: 0,
            len: 0,
            fds: VecDeque::new(),
        }
    }

    pub async fn read_message(
        &mut self,
        ring: &Ring,
        fd: &Rc<OwnedFd>,
    ) -> Result<RxMessage, ClientError> {
        if self.len == 0 {
            self.lo = 0;
        }
        while self.len < 8 {
            self.fill(ring, fd).await?;
        }
        let object = self.word(0);
        let word2 = self.word(4);
        let size = (word2 >> 16) as usize;
        let opcode = word2 & 0xffff;
        if size & 3 != 0 {
            return Err(ClientError::UnalignedMessage);
        }
        if size > MAX_MESSAGE {
            return Err(ClientError::MessageTooLarge);
        }
        if size < 8 {
            return Err(ClientError::MessageTooSmall);
        }
        while self.len < size {
            self.fill(ring, fd).await?;
        }
        let body = self.lo + 8..self.lo + size;
        self.lo += size;
        self.len -= size;
        Ok(RxMessage {
            object: ObjectId(object),
            opcode,
            body,
        })
    }

    /// split borrow: message body and fd queue at once
    pub fn parts(&mut self, body: Range<usize>) -> (&[u8], &mut VecDeque<OwnedFd>) {
        (&self.buf[body], &mut self.fds)
    }

    fn word(&self, at: usize) -> u32 {
        let off = self.lo + at;
        u32::from_ne_bytes(self.buf[off..off + 4].try_into().unwrap())
    }

    async fn fill(&mut self, ring: &Ring, fd: &Rc<OwnedFd>) -> Result<(), ClientError> {
        if self.lo + self.len == self.buf.len() {
            // guaranteed to make room: len < size <= MAX_MESSAGE < RX_SIZE
            self.buf.copy_within(self.lo..self.lo + self.len, 0);
            self.lo = 0;
        }
        let off = self.lo + self.len;
        let got = match ring.recvmsg(fd, mem::take(&mut self.buf), off).await {
            Ok(got) => got,
            Err(RingError::Os(e)) if e == Errno::CONNRESET => return Err(ClientError::Closed),
            Err(e) => return Err(ClientError::Io(e)),
        };
        self.buf = got.buf;
        if got.truncated {
            return Err(ClientError::CmsgTruncated);
        }
        if got.n == 0 {
            return Err(ClientError::Closed);
        }
        self.len += got.n;
        self.fds.extend(got.fds);
        if self.fds.len() > MAX_RX_FDS {
            return Err(ClientError::TooManyFds);
        }
        Ok(())
    }
}

// -- sending --

/// fds pinned to their message's byte offset, so an SCM_RIGHTS payload only
/// ever travels with its own message's first byte
struct MsgFds {
    pos: usize,
    fds: Vec<Rc<OwnedFd>>,
}

#[derive(Default)]
pub struct OutBuffer {
    out: EventOut,
    read_pos: usize,
    fd_groups: VecDeque<MsgFds>,
}

impl OutBuffer {
    /// run one event sender, recording where its fds landed
    fn record(&mut self, f: impl FnOnce(&mut EventOut)) {
        let fds_before = self.out.fds.len();
        let pos = self.out.bytes.len();
        f(&mut self.out);
        if self.out.fds.len() > fds_before {
            let fds = self.out.fds.drain(fds_before..).collect();
            self.fd_groups.push_back(MsgFds { pos, fds });
        }
    }

    fn is_empty(&self) -> bool {
        self.out.bytes.is_empty()
    }

    fn reset(&mut self) {
        self.out.bytes.clear();
        self.out.fds.clear();
        self.read_pos = 0;
        self.fd_groups.clear();
    }
}

#[derive(Default)]
pub struct OutSwapchain {
    cur: OutBuffer,
    pending: VecDeque<OutBuffer>,
    free: Vec<OutBuffer>,
}

impl OutSwapchain {
    #[cfg(test)]
    pub(crate) fn all_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for b in &self.pending {
            out.extend_from_slice(&b.out.bytes);
        }
        out.extend_from_slice(&self.cur.out.bytes);
        out
    }

    pub fn record(&mut self, f: impl FnOnce(&mut EventOut)) {
        self.cur.record(f);
        if self.cur.out.bytes.len() > OUT_FULL {
            self.commit();
        }
    }

    pub fn commit(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let next = self.free.pop().unwrap_or_default();
        let full = mem::replace(&mut self.cur, next);
        self.pending.push_back(full);
    }

    pub fn exceeds_limit(&self) -> bool {
        self.pending.len() > LIMIT_PENDING
    }

    pub fn take_pending(&mut self, into: &mut VecDeque<OutBuffer>) {
        mem::swap(&mut self.pending, into);
    }

    pub fn recycle(&mut self, mut b: OutBuffer) {
        b.reset();
        self.free.push(b);
    }
}

/// drains one buffer through sendmsg, resuming partial writes and keeping each
/// fd group attached to its own message
pub async fn flush_buffer(
    ring: &Ring,
    fd: &Rc<OwnedFd>,
    b: &mut OutBuffer,
    deadline: Time,
) -> Result<(), ClientError> {
    while b.read_pos < b.out.bytes.len() {
        let mut end = b.out.bytes.len();
        let mut fds = Vec::new();
        if let Some(front) = b.fd_groups.front() {
            if front.pos == b.read_pos {
                fds = b.fd_groups.pop_front().unwrap().fds;
                if let Some(next) = b.fd_groups.front() {
                    end = next.pos;
                }
            } else {
                end = front.pos;
            }
        }
        let bytes = mem::take(&mut b.out.bytes);
        match ring.sendmsg(fd, bytes, (b.read_pos, end), fds, Some(deadline)).await {
            Ok((bytes, n)) => {
                b.out.bytes = bytes;
                b.read_pos += n;
            }
            Err(RingError::Os(e)) if e == Errno::CANCELED => return Err(ClientError::Timeout),
            Err(RingError::Os(e)) if e == Errno::CONNRESET || e == Errno::PIPE => {
                return Err(ClientError::Closed);
            }
            Err(e) => return Err(ClientError::Io(e)),
        }
    }
    Ok(())
}
