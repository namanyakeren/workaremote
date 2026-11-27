use anyhow::{anyhow, Result};
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;

pub fn run(tx: &mpsc::Sender<Vec<u8>>, stop: &AtomicBool, enabled: &AtomicBool) -> Result<()> {
    #[cfg(unix)]
    {
        return run_unix(tx, stop, enabled);
    }
    #[cfg(windows)]
    {
        return run_windows(tx, stop, enabled);
    }
    #[allow(unreachable_code)]
    Err(anyhow!("audio tidak didukung pada platform ini"))
}

#[cfg(unix)]
fn run_unix(tx: &mpsc::Sender<Vec<u8>>, stop: &AtomicBool, enabled: &AtomicBool) -> Result<()> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let candidates = [
        (
            "parec",
            vec!["--format=s16le", "--rate=48000", "--channels=1"],
        ),
        (
            "pw-record",
            vec!["--format=s16", "--rate=48000", "--channels=1", "-"],
        ),
        (
            "arecord",
            vec!["-f", "S16_LE", "-r", "48000", "-c", "1", "-t", "raw", "-"],
        ),
    ];
    let (cmd, args) = candidates
        .into_iter()
        .find(|(c, _)| {
            Command::new(c)
                .arg("--help")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok()
        })
        .ok_or_else(|| anyhow!("parec/pw-record/arecord tidak tersedia"))?;
    let mut child = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut out = child.stdout.take().ok_or_else(|| anyhow!("audio stdout"))?;
    let mut seq = 0u16;
    let mut buf = vec![0u8; proto::AUDIO_FRAME];
    while !stop.load(Ordering::Relaxed) && enabled.load(Ordering::Relaxed) {
        let n = out.read(&mut buf)?;
        if n == 0 {
            break;
        }
        tx.blocking_send(proto::encode_audio(seq, &buf[..n]))
            .map_err(|_| anyhow!("audio channel closed"))?;
        seq = seq.wrapping_add(1);
    }
    let _ = child.kill();
    Ok(())
}

#[cfg(windows)]
fn run_windows(tx: &mpsc::Sender<Vec<u8>>, stop: &AtomicBool, enabled: &AtomicBool) -> Result<()> {
    let _ = (tx, stop, enabled);
    Err(anyhow!(
        "audio Windows belum tersedia; gunakan JPEG/input/clipboard"
    ))
}
