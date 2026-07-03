// the per-client task pair - receive in EventHandling, send in PostLayout -
// plus a supervisor that selects on shutdown and deadlines a stuck client
// before the hard kill.

use super::buffers::{self, RxBuffer};
use super::{Client, ClientError};
use crate::engine::Phase;
use crate::protocol::DispatchError;
use crate::protocol::wire::MsgReader;
use crate::util::{OnDrop, Time};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

pub async fn client(data: Rc<Client>) {
    let mut recv = data.state.eng.spawn("client receive", receive(data.clone()));
    let _send = data
        .state
        .eng
        .spawn2("client send", Phase::PostLayout, send(data.clone()));
    // first of: reader finishing, or a shutdown request
    poll_fn(|cx| {
        if Pin::new(&mut recv).poll(cx).is_ready() {
            return Poll::Ready(());
        }
        let mut t = data.shutdown.triggered();
        Pin::new(&mut t).poll(cx)
    })
    .await;
    drop(recv);
    // one last drain for the goodbye, with a hard deadline. usual exit is
    // earlier: peer closes, send task schedules the kill, cancelling us mid-wait.
    data.flush_request.trigger();
    let _ = data.state.wheel.timeout(5000).await;
    crate::trace!("client {} did not shut down in time", data.id);
    data.state.clients.kill(data.id);
}

async fn receive(data: Rc<Client>) {
    let e = match run_receive(&data).await {
        Ok(never) => match never {},
        Err(e) => e,
    };
    if e.peer_closed() {
        crate::trace!("client {} disconnected", data.id);
        data.state.clients.kill(data.id);
    } else {
        crate::trace!("client {}: {}", data.id, e);
        // queues wl_display.error and shuts down; a more specific error queued
        // earlier reaches the client first
        data.implementation_error(&e.to_string());
    }
}

async fn run_receive(data: &Rc<Client>) -> Result<Infallible, ClientError> {
    let sock = data.socket.clone();
    // when we stop reading, shut down the read half so the peer stops writing
    let _shut = OnDrop(move || {
        let _ = rustix::net::shutdown(&*sock, rustix::net::Shutdown::Read);
    });
    let mut rx = RxBuffer::new();
    loop {
        let msg = rx.read_message(&data.state.ring, &data.socket).await?;
        let Some(obj) = data.objects.get(msg.object) else {
            data.invalid_object(msg.object);
            return Err(ClientError::UnknownObject(msg.object));
        };
        let (body, fds) = rx.parts(msg.body);
        let mut r = MsgReader::new(body, fds);
        if let Err(e) = obj.clone().handle_request(msg.opcode, &mut r) {
            if let DispatchError::UnknownOpcode(op) = e {
                data.invalid_request(&*obj, op);
            }
            return Err(ClientError::Dispatch(obj.interface(), obj.id(), e));
        }
    }
}

async fn send(data: Rc<Client>) {
    let sock = data.socket.clone();
    let _shut = OnDrop(move || {
        let _ = rustix::net::shutdown(&*sock, rustix::net::Shutdown::Write);
    });
    let mut bufs = VecDeque::new();
    let err = loop {
        data.flush_request.triggered().await;
        {
            let mut sw = data.swapchain.borrow_mut();
            sw.commit();
            sw.take_pending(&mut bufs);
        }
        // flush unlocked so handlers keep queueing meanwhile
        let deadline = Time::now() + Duration::from_millis(5000);
        let mut failed = None;
        while let Some(mut b) = bufs.pop_front() {
            let r = buffers::flush_buffer(&data.state.ring, &data.socket, &mut b, deadline).await;
            data.swapchain.borrow_mut().recycle(b);
            if let Err(e) = r {
                failed = Some(e);
                break;
            }
        }
        if let Some(e) = failed {
            while let Some(b) = bufs.pop_front() {
                data.swapchain.borrow_mut().recycle(b);
            }
            break e;
        }
    };
    crate::trace!("client {} send task exiting: {}", data.id, err);
    // kill drops our own holder - bounce out of PostLayout so teardown never
    // mutates state mid-phase
    let st = data.state.clone();
    let st2 = st.clone();
    let id = data.id;
    st.run_toplevel.schedule(move || st2.clients.kill(id));
}
