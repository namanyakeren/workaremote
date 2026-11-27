use worka_server::{app, spawn_reaper, state_path_default, App};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::var("WORKA_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let panel_key = std::env::var("WORKA_PANEL_KEY").unwrap_or_default();
    let enroll = std::env::var("WORKA_ENROLL").unwrap_or_default();
    let public_url = std::env::var("WORKA_PUBLIC_URL").unwrap_or_default();

    if panel_key.is_empty() {
        eprintln!("PERINGATAN: WORKA_PANEL_KEY kosong — API panel dinonaktifkan");
    }
    if enroll.is_empty() {
        eprintln!("PERINGATAN: WORKA_ENROLL kosong — siapa pun bisa mendaftarkan agent");
    }

    let state = App::new(panel_key, enroll, public_url, state_path_default());
    spawn_reaper(state.clone());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("worka-server mendengarkan di http://{addr}");
    axum::serve(listener, app(state)).await?;
    Ok(())
}
