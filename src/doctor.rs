// `carrot doctor`: one run, the whole gpu bring-up story - staged,
// flushed line by line to stderr and a numbered report file, so a
// tester's single launch carries everything a remote fix needs. no
// session, no drm master, no modesetting: the riskiest thing here is
// creating a vulkan device, the same as vulkaninfo.

use std::io::Write;

struct Report {
    file: Option<std::fs::File>,
    path: Option<std::path::PathBuf>,
    /// a second copy right in $HOME: one obvious file to attach
    home: Option<std::fs::File>,
    home_path: Option<std::path::PathBuf>,
}

impl Report {
    fn open() -> Report {
        let mut r = Report { file: None, path: None, home: None, home_path: None };
        if let Some(dir) = crate::crash_dir() {
            if std::fs::create_dir_all(&dir).is_ok() {
                let n = crate::next_report_number(&dir, "carrotDoctor");
                let path = dir.join(format!("carrotDoctor{n}.log"));
                if let Ok(f) = std::fs::File::create(&path) {
                    r.file = Some(f);
                    r.path = Some(path);
                }
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            let path = std::path::PathBuf::from(home).join("carrotDoctor.log");
            if let Ok(f) = std::fs::File::create(&path) {
                r.home = Some(f);
                r.home_path = Some(path);
            }
        }
        r
    }

    fn say(&mut self, line: &str) {
        eprintln!("{line}");
        for f in [&mut self.file, &mut self.home].into_iter().flatten() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

/// every distinct mapped .so, flagging anything from a glibc: a leak
/// means a soname the stub family misses on this system
fn maps_sweep(r: &mut Report) {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        r.say("doctor:   (cannot read /proc/self/maps)");
        return;
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut leaks = 0;
    for line in maps.lines() {
        let Some(path) = line.split_whitespace().nth(5) else { continue };
        if !path.contains(".so") || seen.contains(&path) {
            continue;
        }
        seen.push(path);
        if path.contains("glibc") {
            leaks += 1;
            r.say(&format!("doctor:   GLIBC LEAK: {path}"));
        }
    }
    r.say(&format!(
        "doctor:   {} libraries mapped, {leaks} glibc leaks{}",
        seen.len(),
        if leaks == 0 { " (good)" } else { " - a stub soname is missing, list above" },
    ));
}

fn card_stages(r: &mut Report, path: &std::path::Path) {
    use std::os::fd::AsFd;
    r.say(&format!("doctor: == {}", path.display()));
    let card = match rustix::fs::open(path, rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC, rustix::fs::Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            r.say(&format!("doctor:   open failed: {e} (not in the video group?)"));
            return;
        }
    };
    match crate::render::loader::kernel_driver(card.as_fd()) {
        Ok(d) => r.say(&format!("doctor:   kernel driver: {d}")),
        Err(e) => r.say(&format!("doctor:   kernel driver unknown: {e}")),
    }
    r.say("doctor:   [1/3] driver closure (dlopen, gpu-free)");
    let entry = match crate::render::loader::entry_for(card.as_fd()) {
        Ok(e) => e,
        Err(e) => {
            r.say(&format!("doctor:   [1/3] FAILED: {e:?}"));
            return;
        }
    };
    drop(entry);
    r.say("doctor:   [1/3] ok");
    r.say("doctor:   [2/3] mapped closure sweep");
    maps_sweep(r);
    r.say("doctor:   [3/3] vulkan instance + device");
    match crate::render::vulkan::VkCore::new(card.as_fd()) {
        Ok(core) => {
            r.say(&format!(
                "doctor:   [3/3] ok: \"{}\" queue family {}",
                core.device_name, core.queue_family
            ));
            drop(core);
        }
        Err(e) => r.say(&format!("doctor:   [3/3] FAILED: {e:?}")),
    }
}

// -- crash reporting --
// a segfault inside the driver says nothing by itself; this handler gets
// the fault address, the ip, and the maps snapshot into the report before
// the default action takes the core dump. everything in the handler is
// raw syscalls - no alloc, no std io.

static CRASH_FD: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);

unsafe fn syscall4(nr: i64, a: i64, b: i64, c: i64, d: i64) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

// the kernel calls this to return from the handler frame
#[unsafe(naked)]
unsafe extern "C" fn crash_restorer() {
    core::arch::naked_asm!("mov rax, 15", "syscall") // rt_sigreturn
}

fn put_hex(buf: &mut [u8], mut at: usize, v: u64) -> usize {
    buf[at] = b'0';
    buf[at + 1] = b'x';
    at += 2;
    let digits = b"0123456789abcdef";
    let mut started = false;
    for shift in (0..16).rev() {
        let nib = ((v >> (shift * 4)) & 0xf) as usize;
        if nib != 0 || started || shift == 0 {
            buf[at] = digits[nib];
            at += 1;
            started = true;
        }
    }
    at
}

unsafe extern "C" fn on_crash(sig: i32, info: *mut u8, ctx: *mut u8) {
    // x86_64: si_addr at siginfo+0x10, rip in ucontext.uc_mcontext.gregs[16]
    let addr = unsafe { info.add(0x10).cast::<u64>().read() };
    let ip = unsafe { ctx.add(40 + 16 * 8).cast::<u64>().read() };

    let mut buf = [0u8; 96];
    let msg = b"doctor: CRASH signal ";
    buf[..msg.len()].copy_from_slice(msg);
    let mut at = msg.len();
    buf[at] = b'0' + (sig % 10) as u8;
    if sig >= 10 {
        buf[at] = b'0' + (sig / 10) as u8;
        buf[at + 1] = b'0' + (sig % 10) as u8;
        at += 1;
    }
    at += 1;
    let s = b" addr ";
    buf[at..at + s.len()].copy_from_slice(s);
    at += s.len();
    at = put_hex(&mut buf, at, addr);
    let s = b" ip ";
    buf[at..at + s.len()].copy_from_slice(s);
    at += s.len();
    at = put_hex(&mut buf, at, ip);
    buf[at] = b'\n';
    at += 1;

    let report = CRASH_FD.load(core::sync::atomic::Ordering::Relaxed);
    const SYS_OPEN: i64 = 2;
    const SYS_READ: i64 = 0;
    const SYS_WRITE: i64 = 1;
    unsafe {
        for fd in [2, report] {
            if fd >= 0 {
                syscall4(SYS_WRITE, fd as i64, buf.as_ptr() as i64, at as i64, 0);
            }
        }
        // the maps snapshot names the module the ip fell in
        let path = b"/proc/self/maps\0";
        let maps = syscall4(SYS_OPEN, path.as_ptr() as i64, 0, 0, 0);
        if maps >= 0 {
            let mut chunk = [0u8; 1024];
            loop {
                let n = syscall4(SYS_READ, maps, chunk.as_mut_ptr() as i64, chunk.len() as i64, 0);
                if n <= 0 {
                    break;
                }
                for fd in [2, report] {
                    if fd >= 0 {
                        syscall4(SYS_WRITE, fd as i64, chunk.as_ptr() as i64, n, 0);
                    }
                }
            }
        }
    }
    // SA_RESETHAND already re-armed the default action: returning re-runs
    // the faulting instruction and the second hit takes the core dump
}

fn install_crash_handler(report_fd: i32) {
    CRASH_FD.store(report_fd, core::sync::atomic::Ordering::Relaxed);
    const SYS_RT_SIGACTION: i64 = 13;
    const SA_SIGINFO: u64 = 4;
    const SA_RESTORER: u64 = 0x0400_0000;
    const SA_RESETHAND: u64 = 0x8000_0000;
    #[repr(C)]
    struct KernelSigaction {
        handler: u64,
        flags: u64,
        restorer: u64,
        mask: u64,
    }
    let act = KernelSigaction {
        handler: on_crash as usize as u64,
        flags: SA_SIGINFO | SA_RESTORER | SA_RESETHAND,
        restorer: crash_restorer as usize as u64,
        mask: 0,
    };
    for sig in [4i64, 6, 7, 8, 11] {
        // ill, abrt, bus, fpe, segv
        unsafe {
            syscall4(SYS_RT_SIGACTION, sig, &act as *const _ as i64, 0, 8);
        }
    }
}

pub fn run() -> i32 {
    let mut r = Report::open();
    {
        use std::os::fd::AsRawFd;
        install_crash_handler(r.home.as_ref().map_or(-1, |f| f.as_raw_fd()));
    }
    r.say(&format!(
        "carrot doctor {} (pid {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    ));
    r.say("doctor: if a stage hangs: cat /proc/<pid>/maps, then kill it, send both");
    if let Ok(u) = std::fs::read_to_string("/proc/version") {
        r.say(&format!("doctor: kernel: {}", u.trim()));
    }

    r.say("doctor: cards:");
    let mut cards: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/drm") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("card") && !name.contains('-') {
                let driver = std::fs::read_link(e.path().join("device/driver"))
                    .ok()
                    .and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| "?".into());
                r.say(&format!("doctor:   {name}: {driver}"));
                cards.push(std::path::PathBuf::from("/dev/dri").join(name));
            }
        }
    }
    cards.sort();

    r.say("doctor: vulkan icds discovered:");
    for icd in crate::render::loader::all_icd_libraries() {
        r.say(&format!("doctor:   {}", icd.display()));
    }

    r.say("doctor: taproot family:");
    for (name, env) in [("libc.so.6", "CARROT_LIBC"), ("libm.so.6", "CARROT_LIBM")] {
        match crate::render::loader::taproot_lib(name, env) {
            Ok(p) => r.say(&format!("doctor:   {name}: {}", p.display())),
            Err(_) => r.say(&format!("doctor:   {name}: MISSING (fatal for gpu init)")),
        }
    }
    for name in crate::render::loader::STUB_SONAMES {
        match crate::render::loader::taproot_lib(name, "CARROT_STUB_UNSET") {
            Ok(p) => r.say(&format!("doctor:   {name}: {}", p.display())),
            Err(_) => r.say(&format!("doctor:   {name}: missing (glibc may leak in)")),
        }
    }

    for card in &cards {
        card_stages(&mut r, card);
    }
    if cards.is_empty() {
        r.say("doctor: no /dev/dri cards found");
    }

    match (r.home_path.clone(), r.path.clone()) {
        (Some(h), _) => {
            let line = format!("doctor: report written to {} - send that file", h.display());
            r.say(&line);
        }
        (None, Some(p)) => {
            let line = format!("doctor: report written to {}", p.display());
            r.say(&line);
        }
        (None, None) => r.say("doctor: report file could not be written; copy this output"),
    }
    0
}
