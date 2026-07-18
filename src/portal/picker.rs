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
    /// 0-based; the wire ids ("ws:1") are 1-based like the candidate lines
    Workspace(usize),
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
    let pid = child.id();
    let child = Rc::new(RefCell::new(child));
    let watchdog = state.eng.spawn("picker watchdog", {
        let ring = state.ring.clone();
        async move {
            let deadline = Time::from_nsec(Time::now().nsec() + ANSWER_NS);
            if ring.timeout(deadline).await.is_ok() {
                // the picker runs in its own session (setsid): take the
                // whole group down, a shell wrapper's menu included
                if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
                    let _ = rustix::process::kill_process_group(
                        pid,
                        rustix::process::Signal::KILL,
                    );
                }
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
    if let Some(n) = line.strip_prefix("ws:") {
        return n
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .map(Choice::Workspace);
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
    if types & super::SOURCE_MONITOR != 0 {
        // workspace casts present as monitor streams; they are a carrot
        // extra only the picker can reach
        let d = state.display.borrow();
        let outs = d.as_ref().map(|d| d.outputs.borrow().clone()).unwrap_or_default();
        // every workspace the binds can reach is offerable, visited or
        // not; picking an unborn one creates it like switching would
        let count = state.workspaces.borrow().len().max(bind_reach(state));
        for i in 0..count {
            let slot = state
                .workspaces
                .borrow()
                .get(i)
                .map(|ws| ws.output.get())
                .unwrap_or_else(|| state.focused_output.get());
            let output = outs.get(slot).map(|o| o.conn.name.clone()).unwrap_or_default();
            out.push_str(
                &json!({
                    "kind": "workspace",
                    "id": format!("ws:{}", i + 1),
                    "index": i + 1,
                    "output": output,
                    "active": i == state.active_ws.get(),
                })
                .to_string(),
            );
            out.push('\n');
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

/// highest workspace index any bind reaches, as a count
fn bind_reach(state: &Rc<State>) -> usize {
    use crate::config::Action;
    let cfg = state.config.borrow();
    cfg.binds
        .iter()
        .map(|b| match b.action {
            Action::FocusWorkspace(n) | Action::MoveToWorkspace(n) | Action::SendToWorkspace(n) => {
                n + 1
            }
            _ => 0,
        })
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unvisited_workspaces_are_offered() {
        let (state, _client) = crate::client::test_utils::test_client();
        let mut cfg = (**state.config.borrow()).clone();
        cfg.binds.push(crate::config::Bind {
            mods: 0,
            key: 2,
            action: crate::config::Action::FocusWorkspace(8),
            on_release: false,
            repeat: false,
            allow_when_locked: false,
            cooldown_ms: None,
            title: None,
        });
        *state.config.borrow_mut() = Rc::new(cfg);
        let list = candidates(&state, super::super::SOURCE_MONITOR);
        let n = list.lines().filter(|l| l.contains("\"kind\":\"workspace\"")).count();
        assert_eq!(n, 9, "every bind-reachable workspace is offered");
        assert!(list.contains("\"ws:9\""), "the unborn tail workspace is listed");
    }

    #[test]
    fn choices_parse_and_garbage_cancels() {
        assert!(matches!(parse_choice("o:DP-1"), Some(Choice::Output(n)) if n == "DP-1"));
        assert!(matches!(parse_choice("w:42"), Some(Choice::Window(42))));
        assert!(matches!(parse_choice("ws:3"), Some(Choice::Workspace(2))));
        assert!(parse_choice("").is_none());
        assert!(parse_choice("w:pigeon").is_none());
        assert!(parse_choice("ws:0").is_none());
        assert!(parse_choice("everything").is_none());
    }
}
