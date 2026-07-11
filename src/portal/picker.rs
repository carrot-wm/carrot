// the shell-agnostic consent picker: the configured command receives the
// candidate list as ndjson on stdin and answers with one chosen id on
// stdout ("o:NAME" or "w:IDENT"; empty or EOF cancels). no picker
// configured means the focused target is cast directly.

use crate::state::State;
use crate::util::Time;
use serde_json::json;
use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;

/// a picker that never answers must not wedge the session
const ANSWER_NS: u64 = 120 * 1_000_000_000;

pub enum Choice {
    Output(String),
    Window(u64),
}

pub async fn pick(state: &Rc<State>, cmd: &str, types: u32) -> Option<Choice> {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    // reap strays first, like every other spawn site
    while let Ok(Some(_)) = rustix::process::wait(rustix::process::WaitOptions::NOHANG) {}
    let list = candidates(state, types);
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(cmd).stdin(Stdio::piped()).stdout(Stdio::piped());
    unsafe {
        c.pre_exec(|| {
            crate::sighand::unblock_all_in_child();
            let _ = rustix::process::setsid();
            Ok(())
        });
    }
    let mut child = match c.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("carrot: picker \"{cmd}\": {e}");
            return None;
        }
    };
    // the list is tiny; the pipe buffer swallows it whole, and dropping
    // the handle is the EOF the picker waits for
    if let Some(mut sin) = child.stdin.take() {
        let _ = sin.write_all(list.as_bytes());
    }
    let out: Rc<OwnedFd> = Rc::new(child.stdout.take()?.into());
    let child = Rc::new(RefCell::new(child));
    let watchdog = state.eng.spawn("picker watchdog", {
        let child = child.clone();
        let ring = state.ring.clone();
        async move {
            let deadline = Time::from_nsec(Time::now().nsec() + ANSWER_NS);
            if ring.timeout(deadline).await.is_ok() {
                let _ = child.borrow_mut().kill();
            }
        }
    });
    let mut acc = Vec::new();
    let mut buf = vec![0u8; 1024];
    loop {
        match state.ring.read(&out, buf).await {
            Ok((_, 0)) | Err(_) => break,
            Ok((b, n)) => {
                acc.extend_from_slice(&b[..n]);
                if acc.contains(&b'\n') || acc.len() > 4096 {
                    break;
                }
                buf = b;
            }
        }
    }
    drop(watchdog);
    let _ = child.borrow_mut().try_wait();
    let end = acc.iter().position(|&c| c == b'\n').unwrap_or(acc.len());
    parse_choice(String::from_utf8_lossy(&acc[..end]).trim())
}

fn parse_choice(line: &str) -> Option<Choice> {
    if let Some(name) = line.strip_prefix("o:") {
        return Some(Choice::Output(name.to_string()));
    }
    if let Some(id) = line.strip_prefix("w:") {
        return id.parse().ok().map(Choice::Window);
    }
    None
}

fn candidates(state: &Rc<State>, types: u32) -> String {
    let mut out = String::new();
    if types & super::SOURCE_MONITOR != 0 {
        if let Some(d) = state.display.borrow().as_ref() {
            for o in d.outputs.borrow().iter() {
                let (x, y) = o.pos.get();
                out.push_str(
                    &json!({
                        "kind": "output",
                        "id": format!("o:{}", o.conn.name),
                        "name": o.conn.name,
                        "width": o.width,
                        "height": o.height,
                        "x": x,
                        "y": y,
                    })
                    .to_string(),
                );
                out.push('\n');
            }
        }
    }
    if types & super::SOURCE_WINDOW != 0 {
        for (i, ws) in state.workspaces.borrow().iter().enumerate() {
            ws.for_each(|w| {
                if !w.surface().mapped.get() {
                    return;
                }
                out.push_str(
                    &json!({
                        "kind": "window",
                        "id": format!("w:{}", w.ident),
                        "app_id": w.app_id(),
                        "title": w.title(),
                        "workspace": i + 1,
                    })
                    .to_string(),
                );
                out.push('\n');
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_parse_and_garbage_cancels() {
        assert!(matches!(parse_choice("o:DP-1"), Some(Choice::Output(n)) if n == "DP-1"));
        assert!(matches!(parse_choice("w:42"), Some(Choice::Window(42))));
        assert!(parse_choice("").is_none());
        assert!(parse_choice("w:pigeon").is_none());
        assert!(parse_choice("everything").is_none());
    }
}
