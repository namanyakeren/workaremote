//! End-to-end relay test: register an agent, open a session through the panel
//! API, accept it, and move one tile and one input event in each direction.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn next_json(ws: &mut Ws) -> Value {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(&t).unwrap(),
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

#[tokio::test]
async fn agent_viewer_roundtrip() {
    let dir = std::env::temp_dir().join(format!(
        "worka-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let state = worka_server::App::new(
        "panel-key".into(),
        "enroll-me".into(),
        "https://remote.test".into(),
        dir.join("state.json"),
    );
    let router = worka_server::app(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = router.clone();
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });

    // --- agent registers -------------------------------------------------
    let (mut agent, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/agent"))
        .await
        .unwrap();
    agent
        .send(Message::Text(
            json!({
                "t": "register",
                "agent_id": "123456789",
                "secret": "s3cret",
                "enroll": "enroll-me",
                "hostname": "pc-uji",
                "os": "linux x86_64",
                "version": "0.1.0"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut agent).await["t"], "registered");

    // --- wrong panel key is rejected -------------------------------------
    let bad = Request::builder()
        .method("POST")
        .uri("/api/v1/sessions")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"agent_id":"123456789"}"#))
        .unwrap();
    let res = tower::ServiceExt::oneshot(router.clone(), bad)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // --- panel opens a session -------------------------------------------
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/sessions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer panel-key")
        .body(Body::from(r#"{"agent_id":"123 456 789"}"#))
        .unwrap();
    let res = tower::ServiceExt::oneshot(router.clone(), req)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let created: Value = serde_json::from_slice(&body).unwrap();
    let sid = created["session_id"].as_str().unwrap().to_string();
    let token = created["token"].as_str().unwrap().to_string();
    assert!(created["url"]
        .as_str()
        .unwrap()
        .starts_with("https://remote.test/v#sid="));

    // --- viewer connects --------------------------------------------------
    let (mut viewer, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/ws/viewer?sid={sid}&token={token}"))
            .await
            .unwrap();
    assert_eq!(next_json(&mut viewer).await["t"], "waiting");

    let request = next_json(&mut agent).await;
    assert_eq!(request["t"], "session_request");
    assert_eq!(request["sid"], sid.as_str());

    agent
        .send(Message::Text(
            json!({"t": "answer", "sid": sid, "accept": true}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut viewer).await["t"], "connected");

    // --- one tile down, one input up -------------------------------------
    agent.send(Message::Binary(vec![1u8, 0, 0])).await.unwrap();
    match viewer.next().await {
        Some(Ok(Message::Binary(b))) => assert_eq!(b, vec![1u8, 0, 0]),
        other => panic!("expected binary tile, got {other:?}"),
    }

    viewer
        .send(Message::Binary(proto::encode_file_chunk("upload1", b"x")))
        .await
        .unwrap();
    match agent.next().await {
        Some(Ok(Message::Binary(b))) => assert_eq!(
            proto::decode_file_chunk(&b),
            Some(("upload1".into(), &b"x"[..]))
        ),
        other => panic!("expected upload binary, got {other:?}"),
    }

    viewer
        .send(Message::Text(
            json!({"t": "move", "x": 0.25, "y": 0.75}).to_string(),
        ))
        .await
        .unwrap();
    let input = next_json(&mut agent).await;
    assert_eq!(input["t"], "move");
    assert_eq!(input["x"], 0.25);

    // --- the viewer token is single use ----------------------------------
    let again =
        tokio_tungstenite::connect_async(format!("ws://{addr}/ws/viewer?sid={sid}&token={token}"))
            .await;
    assert!(again.is_err(), "token should not work twice");
}
