#![cfg(windows)]

use anyhow::{bail, Result};
/// Refuse installation until a session-aware SYSTEM helper is available.
pub fn command(args: &[String]) -> Result<()> {
    let _ = args;
    bail!(
        "service SYSTEM/helper per-sesi belum tersedia; tidak ada perubahan service yang dilakukan"
    )
}
