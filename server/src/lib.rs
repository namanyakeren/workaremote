//! Relay server: agents dial out to it, the browser viewer dials in, and the
//! server pairs them. It also exposes the small REST API the central panel uses
//! to open a session in a new tab.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use proto::{AgentMsg, ServerToAgent, ViewerEvent};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

const VIEWER_HTML: &str = include_str!("viewer.html");
const SESSION_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRec {
    pub secret: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub last_seen: u64,
}

pub struct Session {
    token: String,
    agent_id: String,
    password: Option<String>,
    viewer: Option<UnboundedSender<Message>>,
    active: bool,
    created: Instant,
}

pub struct App {
    pub panel_key: String,
    pub enroll: String,
    pub public_url: String,
    pub agents: Mutex<HashMap<String, AgentRec>>,
    pub online: Mutex<HashMap<String, UnboundedSender<Message>>>,
    pub sessions: Mutex<HashMap<String, Session>>,
    state_path: PathBuf,
}

impl App {
    pub fn new(panel_key: String, enroll: String, public_url: String, path: PathBuf) -> Arc<App> {
        let agents: HashMap<String, AgentRec> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Arc::new(App {
            panel_key,
            enroll,
            public_url,
            agents: Mutex::new(agents),
            online: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            state_path: path,
        })
    }

    fn persist(&self) {
        let snapshot = {
            let agents = self.agents.lock().unwrap();
            serde_json::to_vec_pretty(&*agents)
        };
        if let (Ok(bytes), Some(dir)) = (snapshot, self.state_path.parent()) {
            let _ = std::fs::create_dir_all(dir);
            // Write then rename: a crash mid-write must not lose every agent secret.
            let tmp = self.state_path.with_extension("tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &self.state_path);
            }
        }
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        if self.panel_key.is_empty() {
            return false;
        }
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                v.strip_prefix("Bearer ")
                    .map(|t| t == self.panel_key)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// Every viewer channel attached to this agent.
    fn viewer_channels(&self, agent_id: &str, only_active: bool) -> Vec<UnboundedSender<Message>> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .values()
            .filter(|s| s.agent_id == agent_id && (!only_active || s.active))
            .filter_map(|s| s.viewer.clone())
            .collect()
    }
}

pub fn app(state: Arc<App>) -> Router {
    Router::new()
        .route("/v", get(viewer_page))
        .route("/api/v1/health", get(health))
        .route("/api/v1/agents", get(list_agents))
        .route("/api/v1/sessions", post(create_session))
        .route("/ws/agent", get(ws_agent))
        .route("/ws/viewer", get(ws_viewer))
        .with_state(state)
}

async fn viewer_page() -> Html<&'static str> {
    Html(VIEWER_HTML)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}))
}

async fn list_agents(State(st): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !st.authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "panel key salah").into_response();
    }
    let agents = st.agents.lock().unwrap();
    let online = st.online.lock().unwrap();
    let list: Vec<_> = agents
        .iter()
        .map(|(id, a)| {
            json!({
                "agent_id": id,
                "hostname": a.hostname,
                "os": a.os,
                "version": a.version,
                "last_seen": a.last_seen,
                "online": online.contains_key(id),
            })
        })
        .collect();
    Json(json!({"agents": list})).into_response()
}

#[derive(Deserialize)]
struct CreateReq {
    agent_id: String,
    #[serde(default)]
    password: Option<String>,
}

async fn create_session(
    State(st): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<CreateReq>,
) -> Response {
    if !st.authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "panel key salah").into_response();
    }
    let agent_id: String = body
        .agent_id
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    if !st.agents.lock().unwrap().contains_key(&agent_id) {
        return (StatusCode::NOT_FOUND, "agent tidak dikenal").into_response();
    }
    if !st.online.lock().unwrap().contains_key(&agent_id) {
        return (StatusCode::CONFLICT, "agent sedang offline").into_response();
    }

    let sid: String = rand_string(24);
    let token: String = rand_string(32);
    st.sessions.lock().unwrap().insert(
        sid.clone(),
        Session {
            token: token.clone(),
            agent_id,
            password: body.password,
            viewer: None,
            active: false,
            created: Instant::now(),
        },
    );

    let base = if st.public_url.is_empty() {
        let host = headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost");
        let scheme = headers
            .get("x-forwarded-proto")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("https");
        format!("{scheme}://{host}")
    } else {
        st.public_url.clone()
    };
    Json(json!({
        "session_id": sid,
        "token": token,
        "url": format!("{base}/v#sid={sid}&token={token}"),
    }))
    .into_response()
}

async fn ws_agent(ws: WebSocketUpgrade, State(st): State<Arc<App>>) -> Response {
    ws.on_upgrade(move |socket| agent_socket(socket, st))
}

async fn agent_socket(mut socket: WebSocket, st: Arc<App>) {
    let Some(Ok(Message::Text(first))) = socket.recv().await else {
        return;
    };
    let Ok(AgentMsg::Register {
        agent_id,
        secret,
        enroll,
        hostname,
        os,
        version,
        elevated: _,
    }) = serde_json::from_str::<AgentMsg>(&first)
    else {
        return;
    };

    let reject = |code: &str, msg: &str| ServerToAgent::Reject {
        code: code.into(),
        msg: msg.into(),
    };
    if !st.enroll.is_empty() && enroll != st.enroll {
        let _ = socket
            .send(Message::Text(
                serde_json::to_string(&reject("enroll", "enrollment token salah")).unwrap(),
            ))
            .await;
        return;
    }
    let verdict: Option<(&'static str, &'static str)> = {
        let mut agents = st.agents.lock().unwrap();
        match agents.get_mut(&agent_id) {
            Some(rec) if rec.secret != secret => Some(("id_taken", "ID sudah dipakai mesin lain")),
            Some(rec) => {
                rec.hostname = hostname;
                rec.os = os;
                rec.version = version;
                rec.last_seen = now();
                None
            }
            None => {
                agents.insert(
                    agent_id.clone(),
                    AgentRec {
                        secret,
                        hostname,
                        os,
                        version,
                        last_seen: now(),
                    },
                );
                None
            }
        }
    };
    if let Some((code, msg)) = verdict {
        let reject = reject(code, msg);
        let _ = socket
            .send(Message::Text(serde_json::to_string(&reject).unwrap()))
            .await;
        return;
    }

    st.persist();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    st.online.lock().unwrap().insert(agent_id.clone(), tx);
    eprintln!("[relay] agent online: {agent_id}");

    let registered = ServerToAgent::Registered {
        agent_id: agent_id.clone(),
    };
    let (mut sink, mut stream) = socket.split();
    if sink
        .send(Message::Text(serde_json::to_string(&registered).unwrap()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(AgentMsg::Answer { sid, accept, reason }) =
                        serde_json::from_str::<AgentMsg>(&t)
                    {
                        answer(&st, &sid, accept, reason);
                    } else {
                        // Screen layout or anything else the viewer should see.
                        for ch in st.viewer_channels(&agent_id, false) {
                            let _ = ch.send(Message::Text(t.clone()));
                        }
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    for ch in st.viewer_channels(&agent_id, true) {
                        let _ = ch.send(Message::Binary(b.clone()));
                    }
                }
                Some(Ok(Message::Ping(p))) => {
                    let _ = sink.send(Message::Pong(p)).await;
                }
                Some(Ok(_)) => {}
                _ => break,
            },
            Some(out) = rx.recv() => {
                if sink.send(out).await.is_err() {
                    break;
                }
            }
        }
    }

    st.online.lock().unwrap().remove(&agent_id);
    if let Some(rec) = st.agents.lock().unwrap().get_mut(&agent_id) {
        rec.last_seen = now();
    }
    st.persist();

    // Tell viewers this machine is gone.
    let mut sessions = st.sessions.lock().unwrap();
    let dead: Vec<String> = sessions
        .iter()
        .filter(|(_, s)| s.agent_id == agent_id)
        .map(|(id, _)| id.clone())
        .collect();
    for sid in dead {
        if let Some(s) = sessions.remove(&sid) {
            if let Some(ch) = s.viewer {
                let _ = ch.send(json_msg(&ViewerEvent::End {
                    reason: "agent terputus".into(),
                }));
            }
        }
    }
    eprintln!("[relay] agent offline: {agent_id}");
}

fn answer(st: &Arc<App>, sid: &str, accept: bool, reason: Option<String>) {
    let mut remove = false;
    let ch = {
        let mut sessions = st.sessions.lock().unwrap();
        let Some(s) = sessions.get_mut(sid) else {
            return;
        };
        let Some(ch) = s.viewer.clone() else { return };
        if accept {
            s.active = true;
        } else if reason.as_deref() != Some("password") {
            remove = true;
        }
        ch
    };
    if remove {
        st.sessions.lock().unwrap().remove(sid);
    }
    if accept {
        let _ = ch.send(json_msg(&ViewerEvent::Connected));
    } else if reason.as_deref() == Some("password") {
        let _ = ch.send(json_msg(&ViewerEvent::NeedPassword));
    } else {
        let _ = ch.send(json_msg(&ViewerEvent::Denied {
            reason: reason.unwrap_or_else(|| "ditolak".into()),
        }));
    }
}

async fn ws_viewer(
    ws: WebSocketUpgrade,
    State(st): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (Some(sid), Some(token)) = (q.get("sid").cloned(), q.get("token").cloned()) else {
        return (StatusCode::BAD_REQUEST, "sid/token kurang").into_response();
    };
    let Some(agent_id) = take_session(&st, &sid, &token) else {
        return (StatusCode::FORBIDDEN, "sesi atau token tidak valid").into_response();
    };
    ws.on_upgrade(move |socket| viewer_socket(socket, st, sid, agent_id))
}

/// Validates and burns the one-shot viewer token.
fn take_session(st: &Arc<App>, sid: &str, token: &str) -> Option<String> {
    let mut sessions = st.sessions.lock().unwrap();
    let s = sessions.get_mut(sid)?;
    if s.token.is_empty()
        || s.token != token
        || s.viewer.is_some()
        || s.created.elapsed() > SESSION_TTL
    {
        return None;
    }
    s.token.clear();
    Some(s.agent_id.clone())
}

async fn viewer_socket(socket: WebSocket, st: Arc<App>, sid: String, agent_id: String) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let password = {
        let mut sessions = st.sessions.lock().unwrap();
        let Some(s) = sessions.get_mut(&sid) else {
            return;
        };
        s.viewer = Some(tx.clone());
        s.password.clone()
    };

    let agent_tx = st.online.lock().unwrap().get(&agent_id).cloned();
    let Some(agent_tx) = agent_tx else {
        let _ = tx.send(json_msg(&ViewerEvent::End {
            reason: "agent offline".into(),
        }));
        st.sessions.lock().unwrap().remove(&sid);
        return;
    };

    let _ = tx.send(json_msg(&ViewerEvent::Waiting));
    let _ = agent_tx.send(json_msg(&ServerToAgent::SessionRequest {
        sid: sid.clone(),
        from: "panel".into(),
        password,
    }));

    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(t))) => {
                    let pw = serde_json::from_str::<serde_json::Value>(&t)
                        .ok()
                        .filter(|v| v.get("t").and_then(|x| x.as_str()) == Some("password"))
                        .and_then(|v| v.get("password").and_then(|x| x.as_str()).map(str::to_string));
                    if let Some(password) = pw {
                        if let Some(s) = st.sessions.lock().unwrap().get_mut(&sid) {
                            s.password = Some(password.clone());
                        }
                        let _ = agent_tx.send(json_msg(&ServerToAgent::SessionRequest {
                            sid: sid.clone(),
                            from: "panel".into(),
                            password: Some(password),
                        }));
                    } else if is_active(&st, &sid) {
                        let _ = agent_tx.send(Message::Text(t));
                    } else if serde_json::from_str::<serde_json::Value>(&t).ok().and_then(|v| v.get("t").and_then(|x| x.as_str()).map(str::to_owned)).as_deref() == Some("file_accept") {
                        let _ = agent_tx.send(Message::Text(t));
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    if is_active(&st, &sid) {
                        let _ = agent_tx.send(Message::Binary(b));
                    }
                }
                Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                Some(Ok(_)) => {}
                _ => break,
            },
            Some(out) = rx.recv() => {
                if sink.send(out).await.is_err() { break; }
            }
        }
    }

    let was_active = is_active(&st, &sid);
    st.sessions.lock().unwrap().remove(&sid);
    if was_active {
        let _ = agent_tx.send(json_msg(&ServerToAgent::SessionEnd { sid }));
    }
}

fn is_active(st: &Arc<App>, sid: &str) -> bool {
    st.sessions
        .lock()
        .unwrap()
        .get(sid)
        .map(|s| s.active)
        .unwrap_or(false)
}

fn json_msg<T: Serialize>(v: &T) -> Message {
    Message::Text(serde_json::to_string(v).unwrap())
}

fn rand_string(n: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn state_path_default() -> PathBuf {
    std::env::var("WORKA_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new("worka-state.json").to_path_buf())
}

/// Drops sessions nobody ever opened, so the map cannot grow forever.
pub fn spawn_reaper(st: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let mut sessions = st.sessions.lock().unwrap();
            // Keep anything a viewer still holds; only unopened links expire.
            sessions.retain(|_, s| s.viewer.is_some() || s.created.elapsed() < SESSION_TTL);
        }
    });
}
