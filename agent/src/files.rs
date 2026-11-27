//! File transfer. Uploads land in a fixed folder with sanitized names; nothing
//! the viewer sends can choose a path.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// 4 GiB is plenty and keeps a runaway peer from filling the disk.
pub const MAX_SIZE: u64 = 4 << 30;
pub const CHUNK: usize = 64 * 1024;

pub fn download_dir() -> PathBuf {
    let base = std::env::var("USERPROFILE")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Downloads").join("Worka Remote")
}

/// Picks a name that does not overwrite an existing file.
fn unique(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 1..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}{ext}", std::process::id()))
}

pub struct Incoming {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub received: u64,
    file: File,
    path: PathBuf,
    done: bool,
}

impl Incoming {
    pub fn create(id: String, name: String, size: u64) -> Result<Incoming> {
        if size > MAX_SIZE {
            bail!("file terlalu besar");
        }
        let dir = download_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("membuat {}", dir.display()))?;
        let clean = proto::safe_file_name(&name);
        let path = unique(&dir, &clean);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("membuat {}", path.display()))?;
        Ok(Incoming {
            id,
            name: clean,
            size,
            received: 0,
            file,
            path,
            done: false,
        })
    }

    pub fn push(&mut self, data: &[u8]) -> Result<()> {
        self.received += data.len() as u64;
        if self.received > self.size {
            bail!("penerima mengirim lebih dari ukuran yang dijanjikan");
        }
        self.file.write_all(data)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<PathBuf> {
        self.file.flush()?;
        if self.received != self.size {
            bail!(
                "transfer tidak lengkap ({} dari {} byte)",
                self.received,
                self.size
            );
        }
        self.done = true;
        Ok(self.path.clone())
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        // A viewer that disconnects mid-upload must not leave half a file behind.
        if !self.done {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct Outgoing {
    file: File,
    pub size: u64,
    pub name: String,
    pub sent: u64,
}

impl Outgoing {
    pub fn open(path: &str) -> Result<Outgoing> {
        let p = Path::new(path);
        let meta = std::fs::metadata(p).with_context(|| format!("tidak bisa membaca {path}"))?;
        if !meta.is_file() {
            bail!("bukan file biasa");
        }
        if meta.len() > MAX_SIZE {
            bail!("file terlalu besar");
        }
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        Ok(Outgoing {
            file: File::open(p)?,
            size: meta.len(),
            name,
            sent: 0,
        })
    }

    pub fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; CHUNK];
        let n = self.file.read(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n);
        self.sent += n as u64;
        Ok(Some(buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_rejects_traversal_and_short_transfer() {
        let dir = std::env::temp_dir().join(format!("worka-files-{}", std::process::id()));
        std::env::set_var("HOME", &dir);
        std::env::set_var("USERPROFILE", &dir);

        let mut f = Incoming::create("x".into(), "../../evil.txt".into(), 5).unwrap();
        assert!(f.path.ends_with("evil.txt"));
        assert!(f.path.starts_with(download_dir()));
        f.push(b"hello").unwrap();
        let path = f.finish().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");

        let mut short = Incoming::create("y".into(), "a.bin".into(), 10).unwrap();
        short.push(b"abc").unwrap();
        assert!(short.finish().is_err(), "short transfer must be rejected");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
