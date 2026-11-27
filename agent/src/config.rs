use anyhow::{Context, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub agent_id: String,
    pub secret: String,
    /// Base URL of the relay, e.g. `https://remote.example.com`.
    pub server: String,
    /// Enrollment token handed out by the operator. Empty means "accept anyone
    /// who can reach the relay" — only sane on a trusted LAN.
    #[serde(default)]
    pub enroll: String,
    #[serde(default)]
    pub label: String,
    /// Argon2 PHC string. Present means unattended access is enabled.
    #[serde(default)]
    pub password_hash: Option<String>,
}

pub fn default_path() -> PathBuf {
    if let Ok(p) = std::env::var("WORKA_CONFIG") {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata)
                .join("WorkaRemote")
                .join("agent.json");
        }
    }
    #[cfg(unix)]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            return PathBuf::from(xdg).join("worka-remote").join("agent.json");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join(".config")
                .join("worka-remote")
                .join("agent.json");
        }
    }
    PathBuf::from("agent.json")
}

fn new_id() -> String {
    // 9 digits, AnyDesk style. Uniqueness is enforced by the relay on register.
    format!("{:09}", rand::thread_rng().gen_range(0..1_000_000_000u32))
}

fn new_secret() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..40)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

impl Config {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let raw =
                fs::read_to_string(path).with_context(|| format!("membaca {}", path.display()))?;
            return Ok(serde_json::from_str(&raw)?);
        }
        let cfg = Config {
            agent_id: new_id(),
            secret: new_secret(),
            server: "https://localhost:8080".into(),
            enroll: String::new(),
            label: String::new(),
            password_hash: None,
        };
        cfg.save(path)?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn ws_url(&self) -> String {
        let base = self.server.trim_end_matches('/');
        let base = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            base.to_string()
        };
        format!("{base}/ws/agent")
    }

    /// "123456789" -> "123 456 789"
    pub fn display_id(&self) -> String {
        let id = &self.agent_id;
        if id.len() == 9 {
            format!("{} {} {}", &id[0..3], &id[3..6], &id[6..9])
        } else {
            id.clone()
        }
    }
}

pub fn hostname() -> String {
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        return h;
    }
    if let Ok(h) = std::env::var("HOSTNAME") {
        return h;
    }
    fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

pub fn os_name() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}
