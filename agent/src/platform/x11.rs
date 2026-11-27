//! X11 backend: XGetImage for capture, XTest for input.
//!
//! Wayland is not supported (no way to read the screen without a portal dialog
//! every session). Sessions on Wayland desktops must run under XWayland.

use crate::platform::Frame;
use anyhow::{anyhow, bail, Result};
use proto::Monitor;
use std::collections::HashMap;
use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::{ConnectionExt as _, Seg, X11_EXTENSION_NAME as SHM_NAME};
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const MOTION_NOTIFY: u8 = 6;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;

pub struct Platform {
    conn: RustConnection,
    root: Window,
    monitors: Vec<Monitor>,
    /// keysym -> (keycode, needs shift)
    keys: HashMap<u32, (u8, bool)>,
    shift: u8,
    shift_held: bool,
    shm: Option<Shm>,
}

/// MIT-SHM buffer. Reading the screen through the X socket instead costs a full
/// copy of every frame, which is unusable at 1080p.
struct Shm {
    seg: Seg,
    addr: *mut u8,
    size: usize,
}

// The pointer is only touched while holding the Platform mutex.
unsafe impl Send for Platform {}

impl Platform {
    pub fn enter_input_desktop(&mut self) {}
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| anyhow!("X11: {e}"))?;
        let root = conn.setup().roots[screen_num].root;
        let mut p = Platform {
            conn,
            root,
            monitors: Vec::new(),
            keys: HashMap::new(),
            shift: 0,
            shift_held: false,
            shm: None,
        };
        p.monitors = p.query_monitors()?;
        p.with_all();
        p.load_keys()?;
        let need = p
            .monitors
            .iter()
            .map(|m| m.w as usize * m.h as usize * 4)
            .max()
            .unwrap_or(0);
        p.shm = p.setup_shm(need);
        if p.shm.is_none() {
            eprintln!("[agent] MIT-SHM tidak tersedia, capture lewat socket (lebih lambat)");
        }
        Ok(p)
    }

    pub fn monitors(&self) -> Vec<Monitor> {
        self.monitors.clone()
    }

    /// Whole virtual desktop as one rectangle, when there is more than one monitor.
    fn with_all(&mut self) {
        if self.monitors.len() < 2 || self.monitors.iter().any(|m| m.i == proto::MON_ALL) {
            return;
        }
        let x0 = self.monitors.iter().map(|m| m.x).min().unwrap_or(0);
        let y0 = self.monitors.iter().map(|m| m.y).min().unwrap_or(0);
        let x1 = self
            .monitors
            .iter()
            .map(|m| m.x + m.w as i32)
            .max()
            .unwrap_or(0);
        let y1 = self
            .monitors
            .iter()
            .map(|m| m.y + m.h as i32)
            .max()
            .unwrap_or(0);
        for m in self.monitors.iter_mut() {
            m.primary = false;
        }
        self.monitors.push(Monitor {
            i: proto::MON_ALL,
            x: x0,
            y: y0,
            w: (x1 - x0) as u16,
            h: (y1 - y0) as u16,
            primary: true,
            name: "Semua monitor".into(),
        });
    }

    fn query_monitors(&self) -> Result<Vec<Monitor>> {
        let mut out = Vec::new();
        let have_randr = self
            .conn
            .extension_information(x11rb::protocol::randr::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some();

        if have_randr {
            if let Ok(cookie) = self.conn.randr_get_monitors(self.root, true) {
                if let Ok(reply) = cookie.reply() {
                    for (i, m) in reply.monitors.iter().enumerate() {
                        let name = self
                            .conn
                            .get_atom_name(m.name)
                            .ok()
                            .and_then(|c| c.reply().ok())
                            .map(|r| String::from_utf8_lossy(&r.name).to_string())
                            .unwrap_or_default();
                        out.push(Monitor {
                            i: i as u8,
                            x: m.x as i32,
                            y: m.y as i32,
                            w: m.width,
                            h: m.height,
                            primary: m.primary,
                            name,
                        });
                    }
                }
            }
        }

        if out.is_empty() {
            let geo = self.conn.get_geometry(self.root)?.reply()?;
            out.push(Monitor {
                i: 0,
                x: 0,
                y: 0,
                w: geo.width,
                h: geo.height,
                primary: true,
                name: "screen".into(),
            });
        }
        if !out.iter().any(|m| m.primary) {
            out[0].primary = true;
        }
        Ok(out)
    }

    fn load_keys(&mut self) -> Result<()> {
        let setup = self.conn.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        let reply = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)?
            .reply()?;
        let per = reply.keysyms_per_keycode as usize;
        for (i, kc) in (min..=max).enumerate() {
            for j in 0..per.min(2) {
                if let Some(&sym) = reply.keysyms.get(i * per + j) {
                    if sym != 0 {
                        self.keys.entry(sym).or_insert((kc, j == 1));
                    }
                }
            }
        }
        self.shift = self.keys.get(&0xffe1).map(|k| k.0).unwrap_or(0);
        Ok(())
    }

    fn setup_shm(&self, size: usize) -> Option<Shm> {
        if size == 0 {
            return None;
        }
        if self
            .conn
            .extension_information(SHM_NAME)
            .ok()
            .flatten()
            .is_none()
        {
            return None;
        }
        let seg: Seg = self.conn.generate_id().ok()?;
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if shmid < 0 {
            return None;
        }
        let addr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
        // Ask the kernel to reclaim it once we detach.
        unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        if addr as isize == -1 {
            return None;
        }
        let attached = self
            .conn
            .shm_attach(seg, shmid as u32, false)
            .ok()
            .and_then(|c| c.check().ok())
            .is_some();
        if !attached {
            unsafe { libc::shmdt(addr) };
            return None;
        }
        Some(Shm {
            seg,
            addr: addr as *mut u8,
            size,
        })
    }

    pub fn capture(&mut self, mon: u8) -> Result<Frame> {
        let m = self
            .monitors
            .iter()
            .find(|m| m.i == mon)
            .or_else(|| self.monitors.first())
            .ok_or_else(|| anyhow!("tidak ada monitor"))?
            .clone();
        let px = m.w as usize * m.h as usize;

        if let Some(shm) = self.shm.as_ref().filter(|s| s.size >= px * 4) {
            let shot = self
                .conn
                .shm_get_image(
                    self.root,
                    m.x as i16,
                    m.y as i16,
                    m.w,
                    m.h,
                    !0,
                    u8::from(ImageFormat::Z_PIXMAP),
                    shm.seg,
                    0,
                )
                .ok()
                .and_then(|c| c.reply().ok());
            if shot.is_some() {
                let data = unsafe { std::slice::from_raw_parts(shm.addr, px * 4) };
                return Ok(Frame {
                    w: m.w,
                    h: m.h,
                    bgra: data.to_vec(),
                });
            }
        }

        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                m.x as i16,
                m.y as i16,
                m.w,
                m.h,
                !0,
            )?
            .reply()?;
        let bgra = match reply.data.len() {
            n if n == px * 4 => reply.data,
            n if n == px * 3 => {
                // 24bpp packed: expand BGR -> BGRA
                let mut out = Vec::with_capacity(px * 4);
                for c in reply.data.chunks_exact(3) {
                    out.extend_from_slice(&[c[0], c[1], c[2], 255]);
                }
                out
            }
            n => bail!(
                "format piksel X11 tidak didukung ({n} byte, depth {})",
                reply.depth
            ),
        };
        Ok(Frame {
            w: m.w,
            h: m.h,
            bgra,
        })
    }

    pub fn clipboard_read(&mut self) -> Option<String> {
        read_clipboard()
    }

    pub fn clipboard_write(&mut self, text: &str) -> bool {
        write_clipboard(text)
    }

    /// Linux has no equivalent of the Windows secure attention sequence.
    pub fn sas(&mut self) -> Result<()> {
        bail!("Ctrl+Alt+Del hanya tersedia di Windows")
    }

    pub fn pointer_move(&mut self, x: i32, y: i32) -> Result<()> {
        self.conn
            .xtest_fake_input(MOTION_NOTIFY, 0, 0, self.root, x as i16, y as i16, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn button(&mut self, x: i32, y: i32, button: u8, down: bool) -> Result<()> {
        self.pointer_move(x, y)?;
        let xb = match button {
            0 => 1,
            1 => 2,
            2 => 3,
            n => n,
        };
        let kind = if down { BUTTON_PRESS } else { BUTTON_RELEASE };
        self.conn
            .xtest_fake_input(kind, xb, 0, self.root, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn wheel(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        self.pointer_move(x, y)?;
        let click = |p: &mut Self, b: u8| -> Result<()> {
            p.conn
                .xtest_fake_input(BUTTON_PRESS, b, 0, p.root, 0, 0, 0)?;
            p.conn
                .xtest_fake_input(BUTTON_RELEASE, b, 0, p.root, 0, 0, 0)?;
            Ok(())
        };
        // One notch per 100 units of browser delta.
        let mut vy = dy / 100;
        let mut vx = dx / 100;
        while vy > 0 {
            click(self, 4)?;
            vy -= 1;
        }
        while vy < 0 {
            click(self, 5)?;
            vy += 1;
        }
        while vx > 0 {
            click(self, 7)?;
            vx -= 1;
        }
        while vx < 0 {
            click(self, 6)?;
            vx += 1;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn raw_key(&mut self, keycode: u8, down: bool) -> Result<()> {
        let kind = if down { KEY_PRESS } else { KEY_RELEASE };
        self.conn
            .xtest_fake_input(kind, keycode, 0, self.root, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn key(&mut self, code: &str, key: &str, down: bool, _mods: u8) -> Result<()> {
        if code == "ShiftLeft" || code == "ShiftRight" {
            self.shift_held = down;
        }
        let Some((kc, needs_shift)) = self.lookup(code, key) else {
            return Ok(());
        };
        let wrap = needs_shift && !self.shift_held && self.shift != 0 && down;
        if wrap {
            self.raw_key(self.shift, true)?;
        }
        self.raw_key(kc, down)?;
        if wrap {
            self.raw_key(self.shift, false)?;
        }
        Ok(())
    }

    fn lookup(&self, code: &str, key: &str) -> Option<(u8, bool)> {
        let keysym = code_keysym(code)
            .or_else(|| {
                let mut it = key.chars();
                let c = it.next()?;
                if it.next().is_some() {
                    None
                } else {
                    Some(char_keysym(c))
                }
            })
            .or_else(|| key_keysym(key))
            .or_else(|| code_keysym_fallback(code))?;
        self.keys.get(&keysym).copied()
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        if let Some(shm) = self.shm.take() {
            let _ = self.conn.shm_detach(shm.seg);
            let _ = self.conn.flush();
            unsafe {
                libc::shmdt(shm.addr as *const libc::c_void);
            }
        }
    }
}

fn run_tool(cmd: &str, args: &[&str], input: Option<&str>) -> Option<Vec<u8>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(text.as_bytes()).ok()?;
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

/// Wayland first (wl-clipboard), then X11 (xclip/xsel). Missing tools simply
/// mean clipboard sync is off.
fn read_clipboard() -> Option<String> {
    let out = run_tool("wl-paste", &["--no-newline", "--type", "text"], None)
        .or_else(|| run_tool("xclip", &["-selection", "clipboard", "-o"], None))
        .or_else(|| run_tool("xsel", &["--clipboard", "--output"], None))?;
    String::from_utf8(out).ok()
}

fn write_clipboard(text: &str) -> bool {
    run_tool("wl-copy", &["--type", "text"], Some(text)).is_some()
        || run_tool("xclip", &["-selection", "clipboard", "-i"], Some(text)).is_some()
        || run_tool("xsel", &["--clipboard", "--input"], Some(text)).is_some()
}

fn char_keysym(c: char) -> u32 {
    let cp = c as u32;
    if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
        cp
    } else {
        0x0100_0000 + cp
    }
}

/// Keys where the browser's `key` string is ambiguous (numpad digits look like
/// normal digits) or where we need a specific physical key.
fn code_keysym(code: &str) -> Option<u32> {
    let v = match code {
        "ShiftLeft" => 0xffe1,
        "ShiftRight" => 0xffe2,
        "ControlLeft" => 0xffe3,
        "ControlRight" => 0xffe4,
        "AltLeft" => 0xffe9,
        "AltRight" => 0xffea,
        "MetaLeft" | "OSLeft" => 0xffeb,
        "MetaRight" | "OSRight" => 0xffec,
        "CapsLock" => 0xffe5,
        "NumpadAdd" => 0xffab,
        "NumpadSubtract" => 0xffad,
        "NumpadMultiply" => 0xffaa,
        "NumpadDivide" => 0xffaf,
        "NumpadDecimal" => 0xffae,
        "NumpadEnter" => 0xff8d,
        "Numpad0" => 0xffb0,
        "Numpad1" => 0xffb1,
        "Numpad2" => 0xffb2,
        "Numpad3" => 0xffb3,
        "Numpad4" => 0xffb4,
        "Numpad5" => 0xffb5,
        "Numpad6" => 0xffb6,
        "Numpad7" => 0xffb7,
        "Numpad8" => 0xffb8,
        "Numpad9" => 0xffb9,
        _ => return None,
    };
    Some(v)
}

fn key_keysym(key: &str) -> Option<u32> {
    let v = match key {
        "Enter" => 0xff0d,
        "Tab" => 0xff09,
        "Backspace" => 0xff08,
        "Escape" | "Esc" => 0xff1b,
        "Delete" => 0xffff,
        "Insert" => 0xff63,
        "Home" => 0xff50,
        "End" => 0xff57,
        "PageUp" => 0xff55,
        "PageDown" => 0xff56,
        "ArrowLeft" => 0xff51,
        "ArrowUp" => 0xff52,
        "ArrowRight" => 0xff53,
        "ArrowDown" => 0xff54,
        "CapsLock" => 0xffe5,
        "NumLock" => 0xff7f,
        "ScrollLock" => 0xff14,
        "PrintScreen" => 0xff61,
        "Pause" => 0xff13,
        "ContextMenu" => 0xff67,
        "Shift" => 0xffe1,
        "Control" => 0xffe3,
        "Alt" => 0xffe9,
        "AltGraph" => 0xff7e,
        "Meta" => 0xffeb,
        " " => 0x20,
        _ => return None,
    };
    Some(v)
}

fn code_keysym_fallback(code: &str) -> Option<u32> {
    let v = match code {
        "Backquote" => 0x60,
        "Minus" => 0x2d,
        "Equal" => 0x3d,
        "BracketLeft" => 0x5b,
        "BracketRight" => 0x5d,
        "Backslash" => 0x5c,
        "Semicolon" => 0x3b,
        "Quote" => 0x27,
        "Comma" => 0x2c,
        "Period" => 0x2e,
        "Slash" => 0x2f,
        "Space" => 0x20,
        "Enter" => 0xff0d,
        "Tab" => 0xff09,
        "Escape" => 0xff1b,
        _ => return None,
    };
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{CreateGCAux, Pixmap, Rectangle};

    /// Reads a server-side pixmap through the same SHM path used for the screen.
    /// This is what proves the byte layout is right; the real screen can be
    /// black in a headless CI box.
    #[test]
    #[ignore = "butuh display X11"]
    fn shm_reads_pixels_in_bgra() {
        let platform = Platform::new().unwrap();
        let shm = platform.shm.as_ref().expect("MIT-SHM harus tersedia");
        let conn = &platform.conn;
        let pix: Pixmap = conn.generate_id().unwrap();
        conn.create_pixmap(24, pix, platform.root, 64, 64)
            .unwrap()
            .check()
            .unwrap();
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, pix, &CreateGCAux::new().foreground(0x00ff_0000u32))
            .unwrap()
            .check()
            .unwrap();
        conn.poly_fill_rectangle(
            pix,
            gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            }],
        )
        .unwrap()
        .check()
        .unwrap();

        conn.shm_get_image(
            pix,
            0,
            0,
            64,
            64,
            !0,
            u8::from(ImageFormat::Z_PIXMAP),
            shm.seg,
            0,
        )
        .unwrap()
        .reply()
        .unwrap();

        let data = unsafe { std::slice::from_raw_parts(shm.addr, 64 * 64 * 4) };
        // Red in little-endian 32bpp is B=0, G=0, R=255.
        assert_eq!(&data[0..3], &[0, 0, 255], "byte order salah");
    }

    #[test]
    #[ignore = "butuh display X11"]
    fn injects_pointer_moves() {
        let mut platform = Platform::new().unwrap();
        platform.pointer_move(321, 234).unwrap();
        let p = platform
            .conn
            .query_pointer(platform.root)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!((p.root_x, p.root_y), (321, 234));
    }

    #[test]
    #[ignore = "butuh display X11"]
    fn maps_browser_keys() {
        let platform = Platform::new().unwrap();
        let a = platform.lookup("KeyA", "a").expect("KeyA");
        let upper = platform.lookup("KeyA", "A").expect("A");
        assert_eq!(a.0, upper.0, "huruf besar/kecil harus keycode yang sama");
        assert!(!a.1 && upper.1, "huruf besar butuh shift");
        assert!(platform.lookup("Enter", "Enter").is_some());
        assert!(platform.lookup("ArrowLeft", "ArrowLeft").is_some());
        assert!(platform.lookup("Numpad5", "5").is_some());
    }
}
