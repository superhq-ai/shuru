// Derived from voxl (https://github.com/tembo/voxl), Copyright (c) 2026 Tembo,
// under the MIT license: see LICENSE-voxl.

//! Input by XTEST. Keys come as keysyms and are resolved against the
//! keymap of the server; a symbol the layout lacks, or one on a shift level
//! the sender is not holding, goes through a spare keycode remapped for it.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

pub enum InputEvent {
    Move { x: i32, y: i32 },
    /// X button numbers: 1 left, 2 middle, 3 right.
    Button { button: u8, down: bool },
    /// Wheel units, 120 a notch: positive dy scrolls down, dx right.
    Wheel { dx: f64, dy: f64 },
    Key { keysym: u32, down: bool },
    ReleaseAll,
    /// Undo keymap changes before exit.
    Shutdown,
}

enum ShiftFix {
    None,
    /// The symbol is on the shifted level and no Shift is held.
    Add,
    /// The symbol is on the plain level and Shift is held.
    Lift,
}

fn scratch_file(display: &str) -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let d = display.replace([':', '/'], "_");
    std::path::Path::new(&dir).join(format!("shuru-screen-keymap{d}"))
}

/// X keycodes (evdev + 8) with no entry in the DOM code table of Chromium.
const SAFE_SCRATCH: &[u8] = &[
    92, 109, 120, 126, 138, 147, 149, 153, 154, 155, 156, 157, 159, 161, 162, 165, 168, 170, 178,
    183, 184, 185, 186, 203, 204, 205, 206, 207, 208, 210, 211, 213, 219, 220, 221, 222, 223, 224,
    226, 227, 228, 229, 230, 231, 234, 235, 244, 245, 246, 247, 248, 249, 250, 251, 253, 254, 255,
];

const SHIFT_L: u32 = 0xffe1;
const SHIFT_R: u32 = 0xffe2;

const NOTCH: f64 = 120.0;

/// Wheel units not sent yet, 120 a notch. A trackpad sends many small
/// deltas; rounding each to a notch would scroll far too much or not at
/// all, so the rest carries over to the next event.
#[derive(Default)]
struct WheelAcc {
    x: f64,
    y: f64,
}

impl WheelAcc {
    /// Adds a delta and takes out whole multiples of `unit`.
    fn take(&mut self, dx: f64, dy: f64, unit: f64) -> (i32, i32) {
        self.x += dx;
        self.y += dy;
        let (nx, ny) = ((self.x / unit).trunc(), (self.y / unit).trunc());
        self.x -= nx * unit;
        self.y -= ny * unit;
        (nx as i32, ny as i32)
    }
}

/// Smooth scrolling through relative motion on the scroll axes of the
/// XTEST pointer. Stock X servers give that device no scroll axes; an Xvfb
/// built with the xtest scroll patch of voxl does.
struct SmoothScroll {
    device: u16,
    vaxis: u8,
    haxis: u8,
    xtest_opcode: u8,
    xi_first_event: u8,
}

impl SmoothScroll {
    fn detect(conn: &RustConnection) -> Option<Self> {
        use x11rb::connection::RequestConnection;
        use x11rb::protocol::xinput::{self, ConnectionExt as _, DeviceClassData, ScrollType};
        let xtest = conn
            .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
            .ok()??;
        let xi = conn
            .extension_information(xinput::X11_EXTENSION_NAME)
            .ok()??;
        conn.xinput_xi_query_version(2, 2).ok()?.reply().ok()?;
        let reply = conn
            .xinput_xi_query_device(xinput::Device::ALL)
            .ok()?
            .reply()
            .ok()?;
        for dev in reply.infos {
            if !String::from_utf8_lossy(&dev.name).contains("XTEST pointer") {
                continue;
            }
            let (mut v, mut h) = (None, None);
            for class in &dev.classes {
                if let DeviceClassData::Scroll(sc) = &class.data {
                    match sc.scroll_type {
                        ScrollType::VERTICAL => v = Some(sc.number as u8),
                        ScrollType::HORIZONTAL => h = Some(sc.number as u8),
                        _ => {}
                    }
                }
            }
            if let (Some(vaxis), Some(haxis)) = (v.filter(|&a| a < 6), h.filter(|&a| a < 6)) {
                return Some(SmoothScroll {
                    device: dev.deviceid,
                    vaxis,
                    haxis,
                    xtest_opcode: xtest.major_opcode,
                    xi_first_event: xi.first_event,
                });
            }
        }
        None
    }

    /// XTestFakeInput with an XI DeviceMotionNotify (relative) and a
    /// DeviceValuator event with the scroll deltas. The FakeInput helper of
    /// x11rb cannot attach valuator events, so the request is built here.
    fn send(&self, conn: &RustConnection, vertical: i32, horizontal: i32) {
        use x11rb::connection::RequestConnection;
        // Always start at axis 0: XTest stores valuators at absolute indices
        // but builds the event mask from the first one.
        let n = self.vaxis.max(self.haxis) as usize + 1;
        let mut vals = [0i32; 6];
        vals[self.vaxis as usize] = vertical;
        vals[self.haxis as usize] = horizontal;
        let mut req = Vec::with_capacity(4 + 64);
        req.extend_from_slice(&[self.xtest_opcode, 2, 17, 0]); // FakeInput, 17 words
        let mut ev = [0u8; 32];
        ev[0] = self.xi_first_event + 5; // XI DeviceMotionNotify
        ev[1] = 1; // relative
        ev[31] = self.device as u8;
        req.extend_from_slice(&ev);
        let mut dv = [0u8; 32];
        dv[0] = self.xi_first_event; // XI DeviceValuator
        dv[1] = self.device as u8;
        dv[6] = n as u8; // num_valuators
        dv[7] = 0; // first_valuator
        for (i, v) in vals.iter().take(n).enumerate() {
            dv[8 + i * 4..12 + i * 4].copy_from_slice(&v.to_ne_bytes());
        }
        req.extend_from_slice(&dv);
        let _ = conn.send_request_without_reply(&[std::io::IoSlice::new(&req)], vec![]);
    }
}

struct Injector {
    conn: RustConnection,
    root: u32,
    min_kc: u8,
    per: usize,
    syms: Vec<u32>,
    lookup: HashMap<u32, (u8, usize)>,
    scratch: Vec<u8>,
    /// keysym to the scratch keycode mapped for it, with its last use.
    scratch_map: HashMap<u32, (u8, Instant)>,
    pressed: HashMap<u32, u8>,
    buttons: [bool; 16],
    shift_down: u32,
    smooth: Option<SmoothScroll>,
    wheel: WheelAcc,
    shift_kc: u8,
    scratch_path: std::path::PathBuf,
}

impl Injector {
    fn new(display: &str) -> Result<Self> {
        let (conn, n) = RustConnection::connect(Some(display))?;
        let root = conn.setup().roots[n].root;
        let min_kc = conn.setup().min_keycode;
        let max_kc = conn.setup().max_keycode;
        let mut me = Injector {
            conn,
            root,
            min_kc,
            per: 0,
            syms: vec![],
            lookup: HashMap::new(),
            scratch: vec![],
            scratch_map: HashMap::new(),
            pressed: HashMap::new(),
            buttons: [false; 16],
            shift_down: 0,
            smooth: None,
            wheel: WheelAcc::default(),
            shift_kc: 0,
            scratch_path: scratch_file(display),
        };
        // Undo remaps left by a run that did not exit cleanly.
        if let Ok(old) = std::fs::read_to_string(&me.scratch_path) {
            me.load_keymap(max_kc)?;
            for kc in old.split_whitespace().filter_map(|k| k.parse::<u8>().ok()) {
                if kc >= min_kc && kc <= max_kc {
                    let _ = me
                        .conn
                        .change_keyboard_mapping(1, kc, me.per as u8, &vec![0; me.per]);
                }
            }
            let _ = me.conn.sync();
            let _ = std::fs::remove_file(&me.scratch_path);
        }
        me.load_keymap(max_kc)?;
        me.shift_kc = me.lookup.get(&SHIFT_L).map(|&(k, _)| k).unwrap_or(50);
        me.smooth = SmoothScroll::detect(&me.conn);
        if me.smooth.is_none() {
            eprintln!("shuru-screen: the XTEST pointer has no scroll axes, scrolling by wheel clicks");
        }
        Ok(me)
    }

    fn load_keymap(&mut self, max_kc: u8) -> Result<()> {
        let count = max_kc - self.min_kc + 1;
        let r = self
            .conn
            .get_keyboard_mapping(self.min_kc, count)?
            .reply()?;
        self.per = r.keysyms_per_keycode as usize;
        self.syms = r.keysyms;
        self.lookup.clear();
        self.scratch.clear();
        for i in 0..count as usize {
            let kc = self.min_kc + i as u8;
            let row = &self.syms[i * self.per..(i + 1) * self.per];
            if row.iter().all(|&s| s == 0) {
                self.scratch.push(kc);
                continue;
            }
            // Only the plain and shift levels are reachable without other
            // modifiers.
            for (lvl, &s) in row.iter().take(2).enumerate() {
                if s != 0 {
                    self.lookup.entry(s).or_insert((kc, lvl));
                }
            }
        }
        // Browsers derive KeyboardEvent.code from the keycode, so a remapped
        // keycode that maps to a real key would act as that key.
        self.scratch.retain(|k| SAFE_SCRATCH.contains(k));
        self.scratch.reverse();
        Ok(())
    }

    fn row(&self, kc: u8) -> &[u32] {
        let i = (kc - self.min_kc) as usize;
        &self.syms[i * self.per..(i + 1) * self.per]
    }

    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16) {
        let _ = self
            .conn
            .xtest_fake_input(ty, detail, x11rb::CURRENT_TIME, self.root, x, y, 0);
    }

    fn resolve(&mut self, keysym: u32) -> (u8, ShiftFix) {
        let want_level = if self.shift_down > 0 { 1 } else { 0 };
        if let Some(&(kc, lvl)) = self.lookup.get(&keysym) {
            let row = self.row(kc);
            let l0 = row[0];
            let l1 = if row.len() > 1 { row[1] } else { 0 };
            let shift_neutral = l1 == 0 || l0 == l1 || (0xfd00..0x10000).contains(&keysym);
            if lvl == want_level || shift_neutral {
                return (kc, ShiftFix::None);
            }
            return (
                kc,
                if lvl == 1 {
                    ShiftFix::Add
                } else {
                    ShiftFix::Lift
                },
            );
        }
        if let Some((kc, t)) = self.scratch_map.get_mut(&keysym) {
            *t = Instant::now();
            return (*kc, ShiftFix::None);
        }
        if self.scratch.is_empty() {
            return (0, ShiftFix::None);
        }
        // Map the keysym onto a spare keycode at every level.
        let kc = if self.scratch_map.len() < self.scratch.len() {
            self.scratch[self.scratch_map.len()]
        } else {
            let (&old, _) = self
                .scratch_map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .unwrap();
            self.scratch_map.remove(&old).unwrap().0
        };
        let row = vec![keysym; self.per];
        let _ = self
            .conn
            .change_keyboard_mapping(1, kc, self.per as u8, &row);
        let _ = self.conn.sync();
        // Clients must take the MappingNotify before they see the key.
        std::thread::sleep(Duration::from_millis(8));
        let i = (kc - self.min_kc) as usize;
        let per = self.per;
        self.syms[i * per..(i + 1) * per].copy_from_slice(&row);
        self.scratch_map.insert(keysym, (kc, Instant::now()));
        let used: Vec<String> = self
            .scratch_map
            .values()
            .map(|(k, _)| k.to_string())
            .collect();
        let _ = std::fs::write(&self.scratch_path, used.join(" "));
        (kc, ShiftFix::None)
    }

    fn restore_keymap(&mut self) {
        for (_, (kc, _)) in std::mem::take(&mut self.scratch_map) {
            let _ = self
                .conn
                .change_keyboard_mapping(1, kc, self.per as u8, &vec![0; self.per]);
        }
        let _ = self.conn.sync();
        let _ = std::fs::remove_file(&self.scratch_path);
    }

    fn click(&self, button: u8, n: i32) {
        for _ in 0..n.min(20) {
            self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0);
            self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0);
        }
    }

    fn handle(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::Move { x, y } => self.fake(
                xproto::MOTION_NOTIFY_EVENT,
                0,
                x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            ),
            InputEvent::Button { button, down } => {
                if button == 0 || button as usize >= self.buttons.len() {
                    return;
                }
                if self.buttons[button as usize] == down {
                    return;
                }
                self.buttons[button as usize] = down;
                let ty = if down {
                    xproto::BUTTON_PRESS_EVENT
                } else {
                    xproto::BUTTON_RELEASE_EVENT
                };
                self.fake(ty, button, 0, 0)
            }
            InputEvent::Wheel { dx, dy } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return;
                }
                if let Some(sm) = &self.smooth {
                    // The scroll axes count 120 a notch too: whole units.
                    let (ux, uy) = self.wheel.take(dx, dy, 1.0);
                    if ux != 0 || uy != 0 {
                        sm.send(&self.conn, uy, ux);
                    }
                    return;
                }
                // X buttons 4 and 5 scroll up and down, 6 and 7 left and right.
                let (nx, ny) = self.wheel.take(dx, dy, NOTCH);
                if ny > 0 {
                    self.click(5, ny);
                } else if ny < 0 {
                    self.click(4, -ny);
                }
                if nx > 0 {
                    self.click(7, nx);
                } else if nx < 0 {
                    self.click(6, -nx);
                }
            }
            InputEvent::Key { keysym, down } => {
                if down {
                    if let Some(&kc) = self.pressed.get(&keysym) {
                        // Autorepeat: X takes a press without a release.
                        self.fake(xproto::KEY_PRESS_EVENT, kc, 0, 0);
                        return;
                    }
                    let (kc, fix) = self.resolve(keysym);
                    if kc == 0 {
                        return;
                    }
                    if keysym == SHIFT_L || keysym == SHIFT_R {
                        self.shift_down += 1;
                    }
                    self.pressed.insert(keysym, kc);
                    match fix {
                        ShiftFix::None => self.fake(xproto::KEY_PRESS_EVENT, kc, 0, 0),
                        ShiftFix::Add => {
                            self.fake(xproto::KEY_PRESS_EVENT, self.shift_kc, 0, 0);
                            self.fake(xproto::KEY_PRESS_EVENT, kc, 0, 0);
                            self.fake(xproto::KEY_RELEASE_EVENT, self.shift_kc, 0, 0);
                        }
                        ShiftFix::Lift => {
                            let held: Vec<u8> = [SHIFT_L, SHIFT_R]
                                .iter()
                                .filter_map(|k| self.pressed.get(k).copied())
                                .collect();
                            for &h in &held {
                                self.fake(xproto::KEY_RELEASE_EVENT, h, 0, 0);
                            }
                            self.fake(xproto::KEY_PRESS_EVENT, kc, 0, 0);
                            for &h in &held {
                                self.fake(xproto::KEY_PRESS_EVENT, h, 0, 0);
                            }
                        }
                    }
                } else if let Some(kc) = self.pressed.remove(&keysym) {
                    if keysym == SHIFT_L || keysym == SHIFT_R {
                        self.shift_down = self.shift_down.saturating_sub(1);
                    }
                    self.fake(xproto::KEY_RELEASE_EVENT, kc, 0, 0);
                }
            }
            InputEvent::ReleaseAll => {
                for (_, kc) in std::mem::take(&mut self.pressed) {
                    self.fake(xproto::KEY_RELEASE_EVENT, kc, 0, 0);
                }
                self.shift_down = 0;
                for b in 1..self.buttons.len() {
                    if self.buttons[b] {
                        self.buttons[b] = false;
                        self.fake(xproto::BUTTON_RELEASE_EVENT, b as u8, 0, 0);
                    }
                }
                self.wheel = WheelAcc::default();
            }
            InputEvent::Shutdown => {
                self.handle(InputEvent::ReleaseAll);
                self.restore_keymap();
            }
        }
    }
}

/// Starts the input thread. Events queued behind one another are applied
/// together, with moves collapsed to the latest.
pub fn spawn(display: &str) -> Result<mpsc::Sender<InputEvent>> {
    let mut inj = Injector::new(display)?;
    let (tx, rx) = mpsc::channel::<InputEvent>();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut pending_move: Option<(i32, i32)> = None;
                let mut apply = |inj: &mut Injector, ev: InputEvent| match ev {
                    InputEvent::Move { x, y } => pending_move = Some((x, y)),
                    other => {
                        if let Some((x, y)) = pending_move.take() {
                            inj.handle(InputEvent::Move { x, y });
                        }
                        inj.handle(other);
                    }
                };
                apply(&mut inj, first);
                while let Ok(ev) = rx.try_recv() {
                    apply(&mut inj, ev);
                }
                if let Some((x, y)) = pending_move {
                    inj.handle(InputEvent::Move { x, y });
                }
                let _ = inj.conn.flush();
            }
        })?;
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_notch_deltas_add_up_to_a_click() {
        let mut w = WheelAcc::default();
        // Four trackpad deltas of a quarter notch: one click, on the last.
        assert_eq!(w.take(0.0, 30.0, NOTCH), (0, 0));
        assert_eq!(w.take(0.0, 30.0, NOTCH), (0, 0));
        assert_eq!(w.take(0.0, 30.0, NOTCH), (0, 0));
        assert_eq!(w.take(0.0, 30.0, NOTCH), (0, 1));
        // Back and forth cancels, nothing is sent.
        assert_eq!(w.take(0.0, -100.0, NOTCH), (0, 0));
        assert_eq!(w.take(0.0, 100.0, NOTCH), (0, 0));
        // Up and left too, with the rest kept.
        assert_eq!(w.take(-250.0, -130.0, NOTCH), (-2, -1));
        assert_eq!(w.take(-10.0, -110.0, NOTCH), (0, -1));
        assert_eq!(w.take(-100.0, 0.0, NOTCH), (-1, 0));
    }

    #[test]
    fn smooth_units_keep_the_fraction() {
        let mut w = WheelAcc::default();
        assert_eq!(w.take(0.0, 2.5, 1.0), (0, 2));
        assert_eq!(w.take(0.0, 2.5, 1.0), (0, 3));
        assert_eq!(w.take(0.4, 0.0, 1.0), (0, 0));
        assert_eq!(w.take(0.6, 0.0, 1.0), (1, 0));
    }
}
