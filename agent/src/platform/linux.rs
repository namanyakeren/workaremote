#[path = "x11.rs"]
mod x11;

use crate::platform::Frame;
use anyhow::{anyhow, bail, Result};
use proto::Monitor;
use std::process::{Command, Stdio};

enum Backend {
    X11(x11::Platform),
    Wayland(Wayland),
}

pub struct Platform {
    backend: Backend,
}

impl Platform {
    pub fn new() -> Result<Self> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            && std::env::var("XDG_SESSION_TYPE").ok().as_deref() == Some("wayland")
        {
            if let Ok(w) = Wayland::new() {
                return Ok(Self {
                    backend: Backend::Wayland(w),
                });
            }
        }
        Ok(Self {
            backend: Backend::X11(x11::Platform::new()?),
        })
    }

    pub fn monitors(&self) -> Vec<Monitor> {
        match &self.backend {
            Backend::X11(p) => p.monitors(),
            Backend::Wayland(p) => p.monitors.clone(),
        }
    }
    pub fn enter_input_desktop(&mut self) {}
    pub fn capture(&mut self, mon: u8) -> Result<Frame> {
        match &mut self.backend {
            Backend::X11(p) => p.capture(mon),
            Backend::Wayland(p) => p.capture(mon),
        }
    }
    pub fn pointer_move(&mut self, x: i32, y: i32) -> Result<()> {
        match &mut self.backend {
            Backend::X11(p) => p.pointer_move(x, y),
            Backend::Wayland(p) => p.mouse(x, y),
        }
    }
    pub fn button(&mut self, x: i32, y: i32, b: u8, down: bool) -> Result<()> {
        match &mut self.backend {
            Backend::X11(p) => p.button(x, y, b, down),
            Backend::Wayland(p) => p.button(x, y, b, down),
        }
    }
    pub fn wheel(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        match &mut self.backend {
            Backend::X11(p) => p.wheel(x, y, dx, dy),
            Backend::Wayland(p) => p.wheel(x, y, dx, dy),
        }
    }
    pub fn key(&mut self, code: &str, key: &str, down: bool, mods: u8) -> Result<()> {
        match &mut self.backend {
            Backend::X11(p) => p.key(code, key, down, mods),
            Backend::Wayland(p) => p.key(code, key, down),
        }
    }
    pub fn clipboard_read(&mut self) -> Option<String> {
        match &mut self.backend {
            Backend::X11(p) => p.clipboard_read(),
            Backend::Wayland(_) => command_text("wl-paste", &["--no-newline"]),
        }
    }
    pub fn clipboard_write(&mut self, text: &str) -> bool {
        match &mut self.backend {
            Backend::X11(p) => p.clipboard_write(text),
            Backend::Wayland(_) => command_input("wl-copy", &["--type", "text"], text),
        }
    }
    pub fn sas(&mut self) -> Result<()> {
        bail!("Ctrl+Alt+Del tidak tersedia pada Linux")
    }
}

struct Wayland {
    monitors: Vec<Monitor>,
}

impl Wayland {
    fn new() -> Result<Self> {
        if !has("grim") {
            bail!("Wayland membutuhkan grim untuk capture")
        }
        let frame = capture_ppm(None)?;
        Ok(Self {
            monitors: vec![Monitor {
                i: 0,
                x: 0,
                y: 0,
                w: frame.0,
                h: frame.1,
                primary: true,
                name: "Wayland".into(),
            }],
        })
    }
    fn capture(&mut self, _mon: u8) -> Result<Frame> {
        let (w, h, rgb) = capture_ppm(None)?;
        let mut bgra = Vec::with_capacity(rgb.len() / 3 * 4);
        for p in rgb.chunks_exact(3) {
            bgra.extend_from_slice(&[p[2], p[1], p[0], 255]);
        }
        Ok(Frame { w, h, bgra })
    }
    fn mouse(&mut self, x: i32, y: i32) -> Result<()> {
        tool(
            "ydotool",
            &["mousemove", "--absolute", &x.to_string(), &y.to_string()],
        )
    }
    fn button(&mut self, x: i32, y: i32, b: u8, down: bool) -> Result<()> {
        self.mouse(x, y)?;
        let code = match b {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 0,
        };
        let click = format!("{:x}", code | if down { 0x40 } else { 0x80 });
        tool("ydotool", &["click", &click])
    }
    fn wheel(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        self.mouse(x, y)?;
        tool("ydotool", &["wheel", &dy.to_string(), &dx.to_string()])
    }
    fn key(&mut self, code: &str, key: &str, down: bool) -> Result<()> {
        let value = if key.chars().count() == 1 {
            key.to_string()
        } else {
            key_name(code).to_string()
        };
        let value = if down { value } else { format!("{value}:0") };
        tool("ydotool", &["key", &value])
    }
}

fn key_name(code: &str) -> &str {
    match code {
        "Enter" => "enter",
        "Tab" => "tab",
        "Backspace" => "backspace",
        "Escape" => "esc",
        "Space" => "space",
        "ArrowUp" => "up",
        "ArrowDown" => "down",
        "ArrowLeft" => "left",
        "ArrowRight" => "right",
        "Delete" => "delete",
        "Home" => "home",
        "End" => "end",
        "PageUp" => "pageup",
        "PageDown" => "pagedown",
        _ => code,
    }
}

fn has(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}
fn tool(cmd: &str, args: &[&str]) -> Result<()> {
    let s = Command::new(cmd)
        .args(args)
        .status()
        .map_err(|e| anyhow!("{cmd}: {e}"))?;
    if s.success() {
        Ok(())
    } else {
        bail!("{cmd} gagal")
    }
}
fn command_text(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).output().ok()?;
    if o.status.success() {
        String::from_utf8(o.stdout).ok()
    } else {
        None
    }
}
fn command_input(cmd: &str, args: &[&str], input: &str) -> bool {
    let mut c = match Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(v) => v,
        Err(_) => return false,
    };
    if let Some(mut i) = c.stdin.take() {
        use std::io::Write;
        let _ = i.write_all(input.as_bytes());
    }
    c.wait().map(|s| s.success()).unwrap_or(false)
}
fn capture_ppm(_geometry: Option<String>) -> Result<(u16, u16, Vec<u8>)> {
    let out = Command::new("grim").args(["-t", "ppm", "-"]).output()?;
    if !out.status.success() {
        bail!("grim gagal")
    }
    let b = &out.stdout;
    let mut i = 0;
    let next = |i: &mut usize| -> Result<String> {
        loop {
            while *i < b.len() && b[*i].is_ascii_whitespace() {
                *i += 1;
            }
            if b.get(*i) == Some(&b'#') {
                while *i < b.len() && b[*i] != b'\n' {
                    *i += 1;
                }
                continue;
            }
            break;
        }
        let s = *i;
        while *i < b.len() && !b[*i].is_ascii_whitespace() {
            *i += 1
        }
        Ok(String::from_utf8(b[s..*i].to_vec())?)
    };
    if next(&mut i)? != "P6" {
        bail!("PPM tidak valid")
    }
    let w: u16 = next(&mut i)?.parse()?;
    let h: u16 = next(&mut i)?.parse()?;
    let max = next(&mut i)?;
    if max != "255" {
        bail!("PPM depth")
    };
    if i < b.len() && b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
        i += 2;
    } else if i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let expected = w as usize * h as usize * 3;
    if b.len().saturating_sub(i) < expected {
        bail!("PPM frame terpotong")
    }
    Ok((w, h, b[i..i + expected].to_vec()))
}
