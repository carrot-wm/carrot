// kbvm keymap: built from RMLVO names, serialized once into a sealed memfd
// for wl_keyboard.keymap. per-seat state in KbState; process() runs for every
// key before any focus/bind check so modifier state stays correct.

use kbvm::state_machine::{Direction, Event, State, StateMachine};
use kbvm::xkb::diagnostic::{Diagnostic, DiagnosticHandler, DiagnosticKind, Severity};
use kbvm::xkb::rmlvo::Group;
use kbvm::lookup::LookupTable;
use kbvm::{Components, Keycode};
use rustix::fs::{MemfdFlags, SealFlags};
use std::os::fd::OwnedFd;
use std::rc::Rc;

/// xkeyboard-config trips kbvm's unimplemented-construct warnings by the
/// thousand; drop them, keep errors.
struct ErrorsOnly;

impl DiagnosticHandler for ErrorsOnly {
    fn filter(&self, kind: DiagnosticKind, _is_fatal: bool) -> bool {
        matches!(kind.severity(), Severity::Error)
    }

    fn handle(&mut self, diag: Diagnostic) {
        eprintln!("carrot: xkb: {}", diag.with_code());
    }
}

pub struct Keymap {
    state_machine: StateMachine,
    lookup: LookupTable,
    pub fd: Rc<OwnedFd>,
    /// includes the trailing nul, per wl_keyboard convention
    pub size: u32,
}

pub struct KbState {
    state: State,
    components: Components,
    events: Vec<Event>,
}

#[derive(Copy, Clone, Default, PartialEq, Eq)]
pub struct Mods {
    pub depressed: u32,
    pub latched: u32,
    pub locked: u32,
    pub group: u32,
}

impl Keymap {
    /// env-driven (XKB_DEFAULT_*); "us" default
    pub fn new_default() -> Result<Rc<Keymap>, String> {
        Keymap::new(None)
    }

    /// a config layout replaces the env layout+variant pair wholesale
    pub fn new(layout: Option<&str>) -> Result<Rc<Keymap>, String> {
        let get = |k: &str| std::env::var(k).ok();
        let rules = get("XKB_DEFAULT_RULES");
        let model = get("XKB_DEFAULT_MODEL");
        let options = get("XKB_DEFAULT_OPTIONS");
        let (layout, variant) = match layout {
            Some(l) => (l.to_string(), String::new()),
            None => match get("XKB_DEFAULT_LAYOUT") {
                Some(l) => (l, get("XKB_DEFAULT_VARIANT").unwrap_or_default()),
                None => ("us".into(), get("XKB_DEFAULT_VARIANT").unwrap_or_default()),
            },
        };

        let mut builder = kbvm::xkb::Context::builder();
        let mut found = false;
        if let Ok(root) = std::env::var("XKB_CONFIG_ROOT") {
            builder.prepend_path(&root);
            found = true;
        } else {
            for path in [
                "/run/current-system/sw/share/X11/xkb",
                "/usr/share/X11/xkb",
                "/etc/X11/xkb",
            ] {
                if std::path::Path::new(path).exists() {
                    builder.prepend_path(path);
                    found = true;
                    break;
                }
            }
        }
        if !found {
            // degenerate keymap maps no modifiers; say why
            return Err(format!(
                "layout \"{layout}\" needs xkb data files - set XKB_CONFIG_ROOT \
                 (nix: ${{xkeyboard-config}}/share/X11/xkb)"
            ));
        }
        let context = builder.build();

        let groups: Vec<Group<'_>> = Group::from_layouts_and_variants(&layout, &variant).collect();
        let options_vec: Option<Vec<&str>> = options.as_deref().map(|o| o.split(',').collect());
        let keymap = context.keymap_from_names(
            ErrorsOnly,
            rules.as_deref(),
            model.as_deref(),
            Some(&groups),
            options_vec.as_deref(),
        );
        Self::from_keymap(&keymap)
    }

    fn from_keymap(keymap: &kbvm::xkb::Keymap) -> Result<Rc<Keymap>, String> {
        let mut bytes = keymap.format().to_string().into_bytes();
        bytes.push(0);
        let size = bytes.len() as u32;
        let fd = rustix::fs::memfd_create(
            "xkb-keymap",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .map_err(|e| format!("keymap memfd: {e}"))?;
        rustix::io::write(&fd, &bytes).map_err(|e| format!("keymap write: {e}"))?;
        rustix::fs::seek(&fd, rustix::fs::SeekFrom::Start(0))
            .map_err(|e| format!("keymap seek: {e}"))?;
        rustix::fs::fcntl_add_seals(
            &fd,
            SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE,
        )
        .map_err(|e| format!("keymap seal: {e}"))?;

        let state_machine = keymap.to_builder().build_state_machine();
        let lookup = keymap.to_builder().build_lookup_table();
        Ok(Rc::new(Keymap {
            state_machine,
            lookup,
            fd: Rc::new(fd),
            size,
        }))
    }

    pub fn create_state(&self) -> KbState {
        KbState {
            state: self.state_machine.create_state(),
            components: Components::default(),
            events: Vec::new(),
        }
    }

    /// does this key auto-repeat in the current group
    pub fn repeats(&self, keycode: u32, group: u32) -> bool {
        self.lookup
            .lookup(kbvm::GroupIndex(group), Default::default(), Keycode::from_evdev(keycode))
            .repeats()
    }
}

impl KbState {
    /// feed one key edge through xkb; Some(mods) when mod/group state changed
    pub fn process(&mut self, map: &Keymap, keycode: u32, pressed: bool) -> Option<Mods> {
        let dir = if pressed {
            Direction::Down
        } else {
            Direction::Up
        };
        self.events.clear();
        map.state_machine.handle_key(
            &mut self.state,
            &mut self.events,
            Keycode::from_evdev(keycode),
            dir,
        );
        let mut changed = false;
        for event in &self.events {
            self.components.apply_event(*event);
            match event {
                Event::ModsPressed(_)
                | Event::ModsLatched(_)
                | Event::ModsLocked(_)
                | Event::ModsEffective(_)
                | Event::GroupEffective(_)
                | Event::GroupLocked(_) => changed = true,
                _ => {}
            }
        }
        changed.then(|| self.mods())
    }

    pub fn mods(&self) -> Mods {
        Mods {
            depressed: self.components.mods_pressed.0,
            latched: self.components.mods_latched.0,
            locked: self.components.mods_locked.0,
            group: self.components.group.0,
        }
    }

    /// clean slate after vt switch; evdev layer replays keys, this only resets xkb
    pub fn reset(&mut self, map: &Keymap) {
        self.state = map.state_machine.create_state();
        self.components = Components::default();
        self.events.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_LEFTSHIFT: u32 = 42;
    const KEY_A: u32 = 30;

    #[test]
    fn default_keymap_builds_and_tracks_shift() {
        let map = Keymap::new_default().unwrap();
        assert!(map.size > 0);
        let mut st = map.create_state();
        let mods = st.process(&map, KEY_LEFTSHIFT, true).expect("shift changes mods");
        assert_ne!(mods.depressed, 0);
        assert!(st.process(&map, KEY_A, true).is_none());
        assert!(st.process(&map, KEY_A, false).is_none());
        let mods = st.process(&map, KEY_LEFTSHIFT, false).expect("release changes mods");
        assert_eq!(mods.depressed, 0);
        assert!(map.repeats(KEY_A, 0));
        assert!(!map.repeats(KEY_LEFTSHIFT, 0));
    }
}
