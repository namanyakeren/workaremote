//! Windows backend: GDI BitBlt for capture (works down to Windows 7), SendInput
//! for input. DXGI Desktop Duplication is faster but needs Windows 8+.

use crate::platform::Frame;
use anyhow::{anyhow, bail, Result};
use proto::Monitor;
use std::cell::Cell;
use std::ffi::c_void;
use std::mem::size_of;
use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDCW, DeleteDC, DeleteObject,
    EnumDisplayMonitors, GetDIBits, GetMonitorInfoW, SelectObject, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HDC, HMONITOR, MONITORINFO, SRCCOPY,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP,
    DESKTOP_WRITEOBJECTS,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{SetCursorPos, MONITORINFOF_PRIMARY};

thread_local! { static INPUT_DESKTOP: Cell<isize> = Cell::new(0); }

#[link(name = "sas")]
extern "system" {
    /// Secure Attention Sequence. Needs SeTcbPrivilege (SYSTEM) and the
    /// SoftwareSASGeneration policy.
    fn SendSAS(as_user: i32);
}

#[link(name = "user32")]
extern "system" {
    fn SetProcessDPIAware() -> i32;
}

pub struct Platform {
    dc: HDC,
    monitors: Vec<Monitor>,
}

unsafe extern "system" fn enum_monitor(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let list = &mut *(data as *mut Vec<Monitor>);
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(hmon, &mut info) != 0 {
        let r = info.rcMonitor;
        list.push(Monitor {
            i: list.len() as u8,
            x: r.left,
            y: r.top,
            w: (r.right - r.left) as u16,
            h: (r.bottom - r.top) as u16,
            primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
            name: String::new(),
        });
    }
    1
}

impl Platform {
    pub fn enter_input_desktop(&mut self) {
        if !crate::platform::elevated() {
            return;
        }
        unsafe {
            let desk = OpenInputDesktop(
                0,
                0,
                DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_SWITCHDESKTOP,
            );
            if desk == 0 || SetThreadDesktop(desk) == 0 {
                if desk != 0 {
                    CloseDesktop(desk);
                }
                return;
            }
            INPUT_DESKTOP.with(|old| {
                if old.get() != 0 {
                    CloseDesktop(old.get());
                }
                old.set(desk);
            });
        }
    }
    pub fn new() -> Result<Self> {
        unsafe {
            // Without this, GDI reports scaled pixels on high-DPI screens and
            // every click lands in the wrong place.
            SetProcessDPIAware();
        }
        let mut monitors: Vec<Monitor> = Vec::new();
        unsafe {
            EnumDisplayMonitors(
                0,
                std::ptr::null(),
                Some(enum_monitor),
                &mut monitors as *mut _ as LPARAM,
            );
        }
        if monitors.is_empty() {
            bail!("tidak ada monitor terdeteksi");
        }
        monitors.sort_by_key(|m| (m.x, m.y));
        for (i, m) in monitors.iter_mut().enumerate() {
            m.i = i as u8;
        }
        if !monitors.iter().any(|m| m.primary) {
            monitors[0].primary = true;
        }
        let mut platform = Platform { dc: 0, monitors };
        platform.with_all();
        let name: Vec<u16> = "DISPLAY\0".encode_utf16().collect();
        let dc = unsafe {
            CreateDCW(
                name.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if dc == 0 {
            bail!("CreateDCW(DISPLAY) gagal");
        }
        platform.dc = dc;
        Ok(platform)
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

    pub fn clipboard_read(&mut self) -> Option<String> {
        unsafe {
            if OpenClipboard(0) == 0 {
                return None;
            }
            let mut out = None;
            let handle = GetClipboardData(CF_UNICODETEXT as u32);
            if handle != 0 {
                let ptr = GlobalLock(handle as *mut c_void) as *const u16;
                if !ptr.is_null() {
                    let mut len = 0usize;
                    while *ptr.add(len) != 0 && len < 1 << 22 {
                        len += 1;
                    }
                    out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(
                        ptr, len,
                    )));
                    GlobalUnlock(handle as *mut c_void);
                }
            }
            CloseClipboard();
            out
        }
    }

    pub fn clipboard_write(&mut self, text: &str) -> bool {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let bytes = wide.len() * 2;
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if handle.is_null() {
                return false;
            }
            let dst = GlobalLock(handle);
            if dst.is_null() {
                return false;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, dst as *mut u8, bytes);
            GlobalUnlock(handle);
            if OpenClipboard(0) == 0 {
                return false;
            }
            EmptyClipboard();
            let ok = SetClipboardData(CF_UNICODETEXT as u32, handle as isize) != 0;
            CloseClipboard();
            ok
        }
    }

    pub fn sas(&mut self) -> Result<()> {
        if !crate::platform::elevated() {
            bail!("Ctrl+Alt+Del butuh agent sebagai service SYSTEM");
        }
        unsafe { SendSAS(0) };
        Ok(())
    }

    pub fn capture(&mut self, mon: u8) -> Result<Frame> {
        let m = self
            .monitors
            .iter()
            .find(|m| m.i == mon)
            .or_else(|| self.monitors.first())
            .ok_or_else(|| anyhow!("tidak ada monitor"))?
            .clone();
        let (w, h) = (m.w as i32, m.h as i32);
        unsafe {
            let mem = CreateCompatibleDC(self.dc);
            if mem == 0 {
                bail!("CreateCompatibleDC gagal");
            }
            let bmp = CreateCompatibleBitmap(self.dc, w, h);
            let old = SelectObject(mem, bmp);
            let blt = BitBlt(mem, 0, 0, w, h, self.dc, m.x, m.y, SRCCOPY | CAPTUREBLT);

            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h; // negative = top-down rows
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB;

            let mut buf = vec![0u8; (w * h * 4) as usize];
            let lines = GetDIBits(
                mem,
                bmp,
                0,
                h as u32,
                buf.as_mut_ptr() as *mut c_void,
                &mut bi,
                DIB_RGB_COLORS,
            );

            SelectObject(mem, old);
            DeleteObject(bmp);
            DeleteDC(mem);
            if blt == 0 || lines == 0 {
                bail!("BitBlt/GetDIBits gagal (layar terkunci?)");
            }
            Ok(Frame {
                w: m.w,
                h: m.h,
                bgra: buf,
            })
        }
    }

    pub fn pointer_move(&mut self, x: i32, y: i32) -> Result<()> {
        unsafe {
            SetCursorPos(x, y);
        }
        Ok(())
    }

    pub fn button(&mut self, x: i32, y: i32, button: u8, down: bool) -> Result<()> {
        self.pointer_move(x, y)?;
        let flag = match (button, down) {
            (0, true) => MOUSEEVENTF_LEFTDOWN,
            (0, false) => MOUSEEVENTF_LEFTUP,
            (1, true) => MOUSEEVENTF_MIDDLEDOWN,
            (1, false) => MOUSEEVENTF_MIDDLEUP,
            (2, true) => MOUSEEVENTF_RIGHTDOWN,
            (2, false) => MOUSEEVENTF_RIGHTUP,
            _ => return Ok(()),
        };
        send_mouse(flag, 0)
    }

    pub fn wheel(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        self.pointer_move(x, y)?;
        if dy != 0 {
            send_mouse(MOUSEEVENTF_WHEEL, dy as u32)?;
        }
        if dx != 0 {
            send_mouse(MOUSEEVENTF_HWHEEL, dx as u32)?;
        }
        Ok(())
    }

    pub fn key(&mut self, code: &str, key: &str, down: bool, mods: u8) -> Result<()> {
        let mut chars = key.chars();
        let single = match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        };
        // Ctrl/Alt/Meta combinations must go through virtual keys, otherwise
        // Ctrl+C would just type a "c".
        if mods & 0b1110 == 0 {
            if let Some(c) = single {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    send_unicode(*unit, down)?;
                }
                return Ok(());
            }
        }
        let vk = code_to_vk(code)
            .or_else(|| key_to_vk(key))
            .ok_or_else(|| anyhow!("tombol tidak dikenal: {code}"))?;
        send_vk(vk, down)
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        unsafe {
            if self.dc != 0 {
                DeleteDC(self.dc);
            }
        }
    }
}

fn send_input(input: INPUT) -> Result<()> {
    let sent = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
    if sent == 0 {
        bail!("SendInput ditolak (UIPI: target berjalan sebagai admin?)");
    }
    Ok(())
}

fn send_mouse(flags: u32, data: u32) -> Result<()> {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: data,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_input(input)
}

fn send_vk(vk: u16, down: bool) -> Result<()> {
    let scan = unsafe { MapVirtualKeyW(vk as u32, 0) } as u16;
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: vk,
        wScan: scan,
        dwFlags: if down { 0 } else { KEYEVENTF_KEYUP },
        time: 0,
        dwExtraInfo: 0,
    };
    send_input(input)
}

fn send_unicode(unit: u16, down: bool) -> Result<()> {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: 0,
        wScan: unit,
        dwFlags: KEYEVENTF_UNICODE | if down { 0 } else { KEYEVENTF_KEYUP },
        time: 0,
        dwExtraInfo: 0,
    };
    send_input(input)
}

fn code_to_vk(code: &str) -> Option<u16> {
    let v = match code {
        "KeyA" => 0x41,
        "KeyB" => 0x42,
        "KeyC" => 0x43,
        "KeyD" => 0x44,
        "KeyE" => 0x45,
        "KeyF" => 0x46,
        "KeyG" => 0x47,
        "KeyH" => 0x48,
        "KeyI" => 0x49,
        "KeyJ" => 0x4A,
        "KeyK" => 0x4B,
        "KeyL" => 0x4C,
        "KeyM" => 0x4D,
        "KeyN" => 0x4E,
        "KeyO" => 0x4F,
        "KeyP" => 0x50,
        "KeyQ" => 0x51,
        "KeyR" => 0x52,
        "KeyS" => 0x53,
        "KeyT" => 0x54,
        "KeyU" => 0x55,
        "KeyV" => 0x56,
        "KeyW" => 0x57,
        "KeyX" => 0x58,
        "KeyY" => 0x59,
        "KeyZ" => 0x5A,
        "Digit0" => 0x30,
        "Digit1" => 0x31,
        "Digit2" => 0x32,
        "Digit3" => 0x33,
        "Digit4" => 0x34,
        "Digit5" => 0x35,
        "Digit6" => 0x36,
        "Digit7" => 0x37,
        "Digit8" => 0x38,
        "Digit9" => 0x39,
        "F1" => 0x70,
        "F2" => 0x71,
        "F3" => 0x72,
        "F4" => 0x73,
        "F5" => 0x74,
        "F6" => 0x75,
        "F7" => 0x76,
        "F8" => 0x77,
        "F9" => 0x78,
        "F10" => 0x79,
        "F11" => 0x7A,
        "F12" => 0x7B,
        "Enter" | "NumpadEnter" => 0x0D,
        "Tab" => 0x09,
        "Space" => 0x20,
        "Backspace" => 0x08,
        "Escape" => 0x1B,
        "Delete" => 0x2E,
        "Insert" => 0x2D,
        "Home" => 0x24,
        "End" => 0x23,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "ShiftLeft" => 0xA0,
        "ShiftRight" => 0xA1,
        "ControlLeft" => 0xA2,
        "ControlRight" => 0xA3,
        "AltLeft" => 0xA4,
        "AltRight" => 0xA5,
        "MetaLeft" => 0x5B,
        "MetaRight" => 0x5C,
        "CapsLock" => 0x14,
        "NumLock" => 0x90,
        "ScrollLock" => 0x91,
        "PrintScreen" => 0x2C,
        "Pause" => 0x13,
        "ContextMenu" => 0x5D,
        "Numpad0" => 0x60,
        "Numpad1" => 0x61,
        "Numpad2" => 0x62,
        "Numpad3" => 0x63,
        "Numpad4" => 0x64,
        "Numpad5" => 0x65,
        "Numpad6" => 0x66,
        "Numpad7" => 0x67,
        "Numpad8" => 0x68,
        "Numpad9" => 0x69,
        "NumpadAdd" => 0x6B,
        "NumpadSubtract" => 0x6D,
        "NumpadMultiply" => 0x6A,
        "NumpadDivide" => 0x6F,
        "NumpadDecimal" => 0x6E,
        "Minus" => 0xBD,
        "Equal" => 0xBB,
        "BracketLeft" => 0xDB,
        "BracketRight" => 0xDD,
        "Backslash" => 0xDC,
        "Semicolon" => 0xBA,
        "Quote" => 0xDE,
        "Comma" => 0xBC,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Backquote" => 0xC0,
        "IntlBackslash" => 0xE2,
        "IntlRo" => 0xE1,
        _ => return None,
    };
    Some(v)
}

fn key_to_vk(key: &str) -> Option<u16> {
    let v = match key {
        "Enter" => 0x0D,
        "Tab" => 0x09,
        " " => 0x20,
        "Backspace" => 0x08,
        "Escape" => 0x1B,
        "Delete" => 0x2E,
        "Insert" => 0x2D,
        "Home" => 0x24,
        "End" => 0x23,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "Shift" => 0xA0,
        "Control" => 0xA2,
        "Alt" => 0xA4,
        "Meta" => 0x5B,
        _ => return None,
    };
    Some(v)
}
