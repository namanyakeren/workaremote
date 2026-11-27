//! Outbound connection to the relay: register, answer session requests, pump
//! tiles/audio/files up and input down.

use crate::audio;
use crate::capture::Tiler;
use crate::config::{hostname, os_name, Config};
use crate::files::{Incoming, Outgoing};
use crate::platform::{ask_permission, Platform};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use proto::{AgentMsg, Monitor, ServerToAgent, ViewerMsg};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const FRAME: Duration = Duration::from_millis(40);

pub struct Shared {
    pub platform: Mutex<Platform>,
    pub monitors: Mutex<Vec<Monitor>>,
    pub active: AtomicBool,
    /// Set when a new viewer joins an already-sharing session: it needs a full frame.
    pub refresh: AtomicBool,
    pub monitor: AtomicU8,
    pub quality: AtomicU8,
    pub clipboard: AtomicBool,
    pub audio: AtomicBool,
    pub video: AtomicBool,
    /// Last clipboard text seen in either direction, to stop echo loops.
    last_clip: Mutex<String>,
    incoming: Mutex<HashMap<String, Incoming>>,
}

impl Shared {
    pub fn new(platform: Platform) -> Self {
        let monitors = platform.monitors();
        Shared {
            platform: Mutex::new(platform),
            monitors: Mutex::new(monitors),
            active: AtomicBool::new(false),
            refresh: AtomicBool::new(true),
            monitor: AtomicU8::new(0),
            quality: AtomicU8::new(72),
            clipboard: AtomicBool::new(true),
            audio: AtomicBool::new(false),
            video: AtomicBool::new(false),
            last_clip: Mutex::new(String::new()),
            incoming: Mutex::new(HashMap::new()),
        }
    }

    fn reset_session(&self) {
        self.clipboard.store(true, Relaxed);
        self.audio.store(false, Relaxed);
        self.video.store(false, Relaxed);
        *self.last_clip.lock().unwrap() = String::new();
        self.incoming.lock().unwrap().clear();
    }
}

struct Ctx {
    sh: Arc<Shared>,
    ctl: mpsc::UnboundedSender<Message>,
    media: mpsc::Sender<Vec<u8>>,
    input: std_mpsc::Sender<ViewerMsg>,
    downloads: Arc<Mutex<HashMap<String, std_mpsc::SyncSender<bool>>>>,
}

pub async fn connect_and_run(cfg: &Config, sh: Arc<Shared>) -> Result<()> {
    let url = cfg.ws_url();
    let tls = native_tls::TlsConnector::new()?;
    let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(
        &url,
        None,
        false,
        Some(tokio_tungstenite::Connector::NativeTls(tls)),
    )
    .await
    .with_context(|| format!("menghubungi {url}"))?;

    let (mut sink, mut stream) = ws.split();
    let register = AgentMsg::Register {
        agent_id: cfg.agent_id.clone(),
        secret: cfg.secret.clone(),
        enroll: cfg.enroll.clone(),
        hostname: if cfg.label.is_empty() {
            hostname()
        } else {
            cfg.label.clone()
        },
        os: os_name(),
        version: env!("CARGO_PKG_VERSION").into(),
        elevated: crate::platform::elevated(),
    };
    sink.send(Message::Text(serde_json::to_string(&register)?))
        .await?;

    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(msg) = stream.next().await {
            if let Message::Text(t) = msg? {
                match serde_json::from_str::<ServerToAgent>(&t) {
                    Ok(ServerToAgent::Registered { agent_id }) => {
                        eprintln!("[agent] terdaftar sebagai {agent_id}");
                        return Ok(());
                    }
                    Ok(ServerToAgent::Reject { msg, .. }) => bail!("ditolak server: {msg}"),
                    _ => {}
                }
            }
        }
        Err(anyhow!("server menutup koneksi saat registrasi"))
    })
    .await
    .context("registrasi timeout")??;

    let (ctl, mut ctl_rx) = mpsc::unbounded_channel::<Message>();
    let (tiles, mut tile_rx) = mpsc::channel::<Vec<u8>>(4);
    let (media, mut media_rx) = mpsc::channel::<Vec<u8>>(32);
    let (input, input_rx) = std_mpsc::channel::<ViewerMsg>();
    let downloads = Arc::new(Mutex::new(HashMap::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(20));
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let sent = tokio::select! {
                Some(m) = ctl_rx.recv() => sink.send(m).await,
                Some(b) = tile_rx.recv() => sink.send(Message::Binary(b)).await,
                Some(b) = media_rx.recv() => sink.send(Message::Binary(b)).await,
                _ = ping.tick() => sink.send(Message::Ping(Vec::new())).await,
            };
            if sent.is_err() {
                break;
            }
        }
    });

    let capture = {
        let stop = stop.clone();
        let sh = sh.clone();
        let media = media.clone();
        tokio::task::spawn_blocking(move || capture_loop(sh, stop, tiles, media))
    };
    let input_thread = {
        let stop = stop.clone();
        let sh = sh.clone();
        std::thread::spawn(move || input_loop(sh, input_rx, stop))
    };
    let clipboard_thread = {
        let stop = stop.clone();
        let sh = sh.clone();
        let ctl = ctl.clone();
        std::thread::spawn(move || clipboard_loop(sh, ctl, stop))
    };

    let ctx = Ctx {
        sh: sh.clone(),
        ctl: ctl.clone(),
        media: media.clone(),
        input,
        downloads: downloads.clone(),
    };
    let result = read_loop(&mut stream, cfg, &ctx).await;

    stop.store(true, Relaxed);
    writer.abort();
    let _ = capture.await;
    let _ = input_thread.join();
    let _ = clipboard_thread.join();
    sh.active.store(false, Relaxed);
    result
}

async fn read_loop<S>(stream: &mut S, cfg: &Config, ctx: &Ctx) -> Result<()>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = stream.next().await {
        match msg? {
            Message::Text(text) => {
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                match value.get("t").and_then(|v| v.as_str()) {
                    Some("session_request") => {
                        let Ok(ServerToAgent::SessionRequest {
                            sid,
                            from,
                            password,
                        }) = serde_json::from_value::<ServerToAgent>(value)
                        else {
                            continue;
                        };
                        let (ok, reason) = decide(cfg, password, from).await;
                        if ok {
                            start_sharing(&ctx.sh, &ctx.ctl)?;
                        }
                        let answer = AgentMsg::Answer {
                            sid,
                            accept: ok,
                            reason,
                        };
                        ctx.ctl.send(text_msg(&answer))?;
                    }
                    Some("session_end") => {
                        ctx.sh.active.store(false, Relaxed);
                        ctx.sh.reset_session();
                    }
                    Some("file_accept") | Some("file_reject") => {
                        if let Some(id) = value.get("id").and_then(Value::as_str) {
                            if let Some(tx) = ctx.downloads.lock().unwrap().remove(id) {
                                let _ = tx.send(
                                    value.get("t").and_then(Value::as_str) == Some("file_accept"),
                                );
                            }
                        }
                    }
                    Some("registered") | Some("reject") => {}
                    _ => {
                        if let Ok(m) = serde_json::from_value::<ViewerMsg>(value) {
                            handle_viewer(m, ctx);
                        }
                    }
                }
            }
            Message::Binary(buf) => {
                if let Some((id, data)) = proto::decode_file_chunk(&buf) {
                    receive_chunk(&ctx.sh, &id, data);
                }
            }
            Message::Ping(p) => {
                let _ = ctx.ctl.send(Message::Pong(p));
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

fn handle_viewer(msg: ViewerMsg, ctx: &Ctx) {
    let sh = &ctx.sh;
    match msg {
        ViewerMsg::Monitor { i } => {
            sh.monitor.store(i, Relaxed);
            sh.refresh.store(true, Relaxed);
        }
        ViewerMsg::Clipboard { text } => {
            if text.len() > 4 << 20 {
                return;
            }
            *sh.last_clip.lock().unwrap() = text.clone();
            sh.platform.lock().unwrap().clipboard_write(&text);
        }
        ViewerMsg::Sas => {
            let r = sh.platform.lock().unwrap().sas();
            if let Err(e) = r {
                let _ = (
                    e,
                    ctx.ctl
                        .send(text_msg(&AgentMsg::Unsupported { what: "sas".into() })),
                );
            }
        }
        ViewerMsg::Audio { on } => {
            sh.audio.store(on, Relaxed);
            if on {
                #[cfg(windows)]
                let _ = ctx.ctl.send(text_msg(&AgentMsg::Unsupported {
                    what: "audio".into(),
                }));
                #[cfg(unix)]
                if !["parec", "pw-record", "arecord"].iter().any(|c| {
                    std::process::Command::new(c)
                        .arg("--help")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .is_ok()
                }) {
                    let _ = ctx.ctl.send(text_msg(&AgentMsg::Unsupported {
                        what: "audio".into(),
                    }));
                    sh.audio.store(false, Relaxed);
                    return;
                }
                let sh = sh.clone();
                let media = ctx.media.clone();
                let ctl = ctx.ctl.clone();
                std::thread::spawn(move || audio_loop(sh, media, ctl));
            }
        }
        ViewerMsg::Video { codec } => {
            if codec == "jpeg" {
                sh.video.store(false, Relaxed);
            } else {
                sh.video
                    .store(codec == "h264" && crate::video::available(), Relaxed);
            }
            sh.refresh.store(true, Relaxed);
        }
        ViewerMsg::FileOffer { id, name, size } => match Incoming::create(id.clone(), name, size) {
            Ok(f) => {
                let name = f.name.clone();
                sh.incoming.lock().unwrap().insert(id.clone(), f);
                let _ = ctx.ctl.send(text_msg(&AgentMsg::FileAccept { id }));
                eprintln!("[agent] menerima file: {name}");
            }
            Err(e) => {
                let _ = ctx.ctl.send(text_msg(&AgentMsg::FileReject {
                    id,
                    reason: e.to_string(),
                }));
            }
        },
        ViewerMsg::FileGet { id, path } => send_file(id, path, ctx),
        other => {
            // Everything else is input and belongs on the thread that owns the
            // input desktop.
            let _ = ctx.input.send(other);
        }
    }
}

fn receive_chunk(sh: &Arc<Shared>, id: &str, data: &[u8]) {
    let mut map = sh.incoming.lock().unwrap();
    let Some(f) = map.get_mut(id) else { return };
    if let Err(e) = f.push(data) {
        eprintln!("[agent] file: {e}");
        map.remove(id);
        return;
    }
    if f.received == f.size {
        let finished = map.remove(id).unwrap();
        match finished.finish() {
            Ok(path) => eprintln!("[agent] file tersimpan: {}", path.display()),
            Err(e) => eprintln!("[agent] file gagal: {e}"),
        }
    }
}

fn send_file(id: String, path: String, ctx: &Ctx) {
    let ctl = ctx.ctl.clone();
    let media = ctx.media.clone();
    let downloads = ctx.downloads.clone();
    std::thread::spawn(move || {
        let mut file = match Outgoing::open(&path) {
            Ok(f) => f,
            Err(e) => {
                let _ = ctl.send(text_msg(&AgentMsg::FileDone {
                    id,
                    ok: false,
                    error: e.to_string(),
                }));
                return;
            }
        };
        let offer = AgentMsg::FileOffer {
            id: id.clone(),
            name: file.name.clone(),
            size: file.size,
        };
        if ctl.send(text_msg(&offer)).is_err() {
            return;
        }
        let (accept_tx, accept_rx) = std_mpsc::sync_channel(1);
        downloads.lock().unwrap().insert(id.clone(), accept_tx);
        if !accept_rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or(false)
        {
            return;
        }
        loop {
            match file.next_chunk() {
                Ok(Some(chunk)) => {
                    if media
                        .blocking_send(proto::encode_file_chunk(&id, &chunk))
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = ctl.send(text_msg(&AgentMsg::FileDone {
                        id,
                        ok: false,
                        error: e.to_string(),
                    }));
                    return;
                }
            }
        }
        let _ = ctl.send(text_msg(&AgentMsg::FileDone {
            id,
            ok: true,
            error: String::new(),
        }));
    });
}

fn audio_loop(sh: Arc<Shared>, media: mpsc::Sender<Vec<u8>>, ctl: mpsc::UnboundedSender<Message>) {
    let stop = AtomicBool::new(false);
    let mut failures = 0;
    while sh.audio.load(Relaxed) {
        match audio::run(&media, &stop, &sh.audio) {
            Ok(()) => break,
            Err(e) => {
                failures += 1;
                eprintln!("[agent] audio: {e}");
                if failures >= 3 {
                    sh.audio.store(false, Relaxed);
                    let _ = ctl.send(text_msg(&AgentMsg::Unsupported {
                        what: "audio".into(),
                    }));
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn input_loop(sh: Arc<Shared>, rx: std_mpsc::Receiver<ViewerMsg>, stop: Arc<AtomicBool>) {
    while !stop.load(Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(msg) => apply_input(msg, &sh),
            Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
}

fn clipboard_loop(sh: Arc<Shared>, ctl: mpsc::UnboundedSender<Message>, stop: Arc<AtomicBool>) {
    while !stop.load(Relaxed) {
        if sh.active.load(Relaxed) && sh.clipboard.load(Relaxed) {
            let text = sh.platform.lock().unwrap().clipboard_read();
            if let Some(t) = text {
                let changed = {
                    let mut last = sh.last_clip.lock().unwrap();
                    if t != *last && !t.is_empty() && t.len() < 4 << 20 {
                        *last = t.clone();
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    let _ = ctl.send(text_msg(&AgentMsg::Clipboard { text: t }));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(700));
    }
}

fn text_msg<T: serde::Serialize>(v: &T) -> Message {
    Message::Text(serde_json::to_string(v).unwrap())
}

fn start_sharing(sh: &Arc<Shared>, ctl: &mpsc::UnboundedSender<Message>) -> Result<()> {
    let monitors = sh.platform.lock().unwrap().monitors();
    let primary = monitors
        .iter()
        .find(|m| m.primary)
        .map(|m| m.i)
        .unwrap_or(0);
    *sh.monitors.lock().unwrap() = monitors.clone();
    sh.monitor.store(primary, Relaxed);
    sh.reset_session();
    sh.active.store(true, Relaxed);
    sh.refresh.store(true, Relaxed);
    ctl.send(text_msg(&AgentMsg::Screen { monitors }))?;
    Ok(())
}

async fn decide(cfg: &Config, password: Option<String>, from: String) -> (bool, Option<String>) {
    if let Some(hash) = &cfg.password_hash {
        if let Some(pw) = password {
            if verify(hash, &pw) {
                return (true, None);
            }
        }
        return (false, Some("password".into()));
    }
    let ok = tokio::task::spawn_blocking(move || ask_permission(&from))
        .await
        .unwrap_or(false);
    (ok, if ok { None } else { Some("denied".into()) })
}

fn verify(hash: &str, password: &str) -> bool {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

fn apply_input(msg: ViewerMsg, sh: &Arc<Shared>) {
    let mon = sh.monitor.load(Relaxed);
    let rect = {
        let mons = sh.monitors.lock().unwrap();
        mons.iter()
            .find(|m| m.i == mon)
            .or_else(|| mons.first())
            .cloned()
    };
    let Some(rect) = rect else { return };
    let abs = |nx: f32, ny: f32| {
        (
            rect.x + (nx.clamp(0.0, 1.0) * rect.w as f32) as i32,
            rect.y + (ny.clamp(0.0, 1.0) * rect.h as f32) as i32,
        )
    };

    let mut p = sh.platform.lock().unwrap();
    p.enter_input_desktop();
    let result = match msg {
        ViewerMsg::Move { x, y } => {
            let (ax, ay) = abs(x, y);
            p.pointer_move(ax, ay)
        }
        ViewerMsg::Button { x, y, button, down } => {
            let (ax, ay) = abs(x, y);
            p.button(ax, ay, button, down)
        }
        ViewerMsg::Wheel { x, y, dx, dy } => {
            let (ax, ay) = abs(x, y);
            p.wheel(ax, ay, dx, dy)
        }
        ViewerMsg::Key {
            code,
            key,
            down,
            mods,
        } => p.key(&code, &key, down, mods),
        _ => Ok(()),
    };
    if let Err(e) = result {
        eprintln!("[agent] input: {e}");
    }
}

fn capture_loop(
    sh: Arc<Shared>,
    stop: Arc<AtomicBool>,
    tiles: mpsc::Sender<Vec<u8>>,
    media: mpsc::Sender<Vec<u8>>,
) {
    let mut tiler = Tiler::new();
    let mut video = crate::video::Encoder::new();
    let mut last_mon = u8::MAX;
    while !stop.load(Relaxed) {
        if !sh.active.load(Relaxed) {
            tiler.reset();
            video.reset();
            last_mon = u8::MAX;
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if sh.refresh.swap(false, Relaxed) {
            tiler.reset();
            video.reset();
        }
        let mon = sh.monitor.load(Relaxed);
        if mon != last_mon {
            tiler.reset();
            video.reset();
            last_mon = mon;
        }
        let started = Instant::now();
        let frame = {
            let mut p = sh.platform.lock().unwrap();
            p.enter_input_desktop();
            p.capture(mon)
        };
        match frame {
            Ok(f) => {
                if sh.video.load(Relaxed) {
                    if let Some(packet) = video.encode(&f) {
                        if media.blocking_send(packet).is_err() {
                            return;
                        }
                    }
                } else {
                    let batch = tiler.encode(&f, sh.quality.load(Relaxed));
                    if !batch.is_empty() && tiles.try_send(batch).is_err() {
                        // Dropped because the link is behind. Forget what we
                        // "sent" so the next tick re-sends it.
                        tiler.reset();
                    }
                }
            }
            Err(e) => {
                eprintln!("[agent] capture: {e}");
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        }
        let spent = started.elapsed();
        if spent < FRAME {
            std::thread::sleep(FRAME - spent);
        }
    }
}
