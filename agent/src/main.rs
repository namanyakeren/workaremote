mod audio;
mod capture;
mod config;
mod files;
mod net;
mod platform;
#[cfg(windows)]
mod service;
mod video;

use anyhow::{bail, Context, Result};
use config::Config;
use platform::Platform;
use std::path::PathBuf;
use std::sync::Arc;

fn main() -> Result<()> {
    let mut path = config::default_path();
    let mut args: Vec<String> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--config" {
            path = PathBuf::from(it.next().context("--config butuh sebuah path")?);
        } else if a == "-h" || a == "--help" {
            usage();
            return Ok(());
        } else {
            args.push(a);
        }
    }

    let mut cfg = Config::load_or_create(&path)?;
    match args.first().map(String::as_str) {
        #[cfg(windows)]
        Some("service") => service::command(&args[1..])?,
        Some("id") => println!("{}", cfg.display_id()),
        Some("enroll") => {
            let mut server = None;
            let mut token = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--server" => server = args.get(i + 1).cloned(),
                    "--token" => token = args.get(i + 1).cloned(),
                    _ => {}
                }
                i += 2;
            }
            let server = server.context("enroll butuh --server")?;
            if !server.starts_with("http") {
                bail!("--server harus diawali http:// atau https://");
            }
            cfg.server = server;
            if let Some(t) = token {
                cfg.enroll = t;
            }
            cfg.save(&path)?;
            println!("server = {}", cfg.server);
            println!("agent id = {}", cfg.display_id());
        }
        Some("set-password") => {
            let a = rpassword::prompt_password("Password unattended: ")?;
            let b = rpassword::prompt_password("Ulangi password: ")?;
            if a != b {
                bail!("password tidak sama");
            }
            if a.len() < 8 {
                bail!("minimal 8 karakter");
            }
            cfg.password_hash = Some(hash_password(&a)?);
            cfg.save(&path)?;
            println!("password tersimpan di {}", path.display());
        }
        Some("clear-password") => {
            cfg.password_hash = None;
            cfg.save(&path)?;
            println!("password dihapus; koneksi akan meminta konfirmasi");
        }
        Some(other) => {
            bail!("perintah tidak dikenal: {other} (lihat --help)");
        }
        None => run(cfg, path)?,
    }
    Ok(())
}

fn hash_password(pw: &str) -> Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    use argon2::Argon2;
    use rand::RngCore;
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let salt = SaltString::encode_b64(&salt).expect("salt");
    Ok(Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2: {e}"))?
        .to_string())
}

fn run(cfg: Config, path: PathBuf) -> Result<()> {
    platform::set_elevated_from_os();
    let platform = Platform::new()?;
    let shared = Arc::new(net::Shared::new(platform));
    println!("Worka Remote agent");
    println!("  id     : {}", cfg.display_id());
    println!("  server : {}", cfg.server);
    println!(
        "  akses  : {}",
        if cfg.password_hash.is_some() {
            "unattended (password)"
        } else {
            "konfirmasi manual"
        }
    );
    println!("  config : {}", path.display());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        loop {
            if let Err(e) = net::connect_and_run(&cfg, shared.clone()).await {
                eprintln!("[agent] {e:#}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    })
}

fn usage() {
    println!(
        "worka-agent [--config PATH] [perintah]

  (tanpa perintah)  jalankan agent
  id                tampilkan ID mesin ini
  enroll --server URL [--token TOKEN]
                    arahkan agent ke relay
  set-password      aktifkan unattended access
  clear-password    kembali ke mode konfirmasi"
    );
}
