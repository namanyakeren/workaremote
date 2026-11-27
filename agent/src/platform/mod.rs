//! Screen capture and input injection, per platform.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Platform;

#[cfg(unix)]
mod linux;
#[cfg(unix)]
pub use linux::Platform;

use std::sync::atomic::{AtomicBool, Ordering};

static ELEVATED: AtomicBool = AtomicBool::new(false);

/// True when the agent runs with the privileges needed for the secure desktop
/// (Windows service helper as SYSTEM, or root on Linux).
pub fn set_elevated(v: bool) {
    ELEVATED.store(v, Ordering::Relaxed);
}

pub fn elevated() -> bool {
    ELEVATED.load(Ordering::Relaxed)
}

#[cfg(unix)]
pub fn set_elevated_from_os() {
    set_elevated(unsafe { libc::geteuid() == 0 });
}

#[cfg(windows)]
pub fn set_elevated_from_os() {}

/// One captured monitor, BGRA8, tightly packed, top-down.
pub struct Frame {
    pub w: u16,
    pub h: u16,
    pub bgra: Vec<u8>,
}

/// Asks the person in front of the target machine. Blocks until answered.
#[cfg(windows)]
pub fn ask_permission(from: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };
    let text: Vec<u16> = format!("Permintaan kendali jarak jauh dari {from}.\n\nTerima koneksi?")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let title: Vec<u16> = "Worka Remote"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let r = unsafe {
        MessageBoxW(
            0,
            text.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    r == IDYES
}

/// Linux has no built-in dialog toolkit, so borrow whatever the desktop ships.
#[cfg(unix)]
pub fn ask_permission(from: &str) -> bool {
    use std::process::{Command, Stdio};
    let text = format!("Permintaan kendali jarak jauh dari {from}.\n\nTerima koneksi?");
    let text_arg = format!("--text={text}");
    let attempts: Vec<(&str, Vec<&str>)> = vec![
        (
            "zenity",
            vec![
                "--question",
                "--title=Worka Remote",
                &text_arg,
                "--timeout=60",
                "--ok-label=Terima",
                "--cancel-label=Tolak",
            ],
        ),
        ("kdialog", vec!["--title=Worka Remote", "--yesno", &text]),
    ];
    for (cmd, args) in attempts {
        let ok = Command::new(cmd)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Ok(status) = ok {
            return status.success();
        }
    }
    eprintln!("tidak ada dialog (zenity/kdialog); permintaan ditolak");
    false
}
