use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use axum::Json;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::extract::Path;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::extract::ws::Message;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use hmac::Hmac;
use hmac::Mac;
use serde_json::json;
use sha2::Sha256;
use tokio::io::AsyncWriteExt;

use crate::Manifest;
use crate::coordinator::Coordinator;
use crate::model::TeamRequest;

#[derive(Clone)]
struct HttpState {
    coordinator: Arc<Coordinator>,
    token: Arc<String>,
}

pub(crate) async fn serve(manifest: Manifest) -> Result<()> {
    let automatic = std::env::var_os(crate::startup::READY_MARKER).is_some();
    let startup = if automatic {
        match crate::startup::prepare_automatic_start(&manifest).await? {
            Some(startup) => Some(startup),
            None => {
                crate::startup::report_ready(&mut std::io::stdout().lock())?;
                return Ok(());
            }
        }
    } else {
        None
    };
    let path = &manifest.coordinator.token_file;
    if !tokio::fs::try_exists(path).await? {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(path).await?;
        file.write_all(format!("{}{}\n", uuid::Uuid::new_v4(), uuid::Uuid::new_v4()).as_bytes())
            .await?;
        file.sync_all().await?;
    }
    let token = crate::transport::token(Some(path))
        .await?
        .context("operator token file is missing")?;
    let listener = tokio::net::TcpListener::bind(manifest.coordinator.listen).await?;
    let coordinator = Coordinator::open(manifest).await?;
    let _workers = crate::worker::start(&coordinator);
    crate::gitea_publish::start(&coordinator);
    if automatic {
        crate::startup::report_ready(&mut std::io::stdout().lock())?;
    }
    // The child owns serialization through token creation, listener binding and
    // readiness even if the UI that spawned it has already disconnected.
    drop(startup);
    axum::serve(listener, router(coordinator, token))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

pub(crate) fn router(coordinator: Arc<Coordinator>, token: String) -> Router {
    Router::new()
        .route("/rpc", post(rpc))
        .route("/events", get(events))
        .route("/relay/{machine}/{server}", get(relay))
        .route("/gitea/webhook", post(gitea_webhook))
        .layer(DefaultBodyLimit::disable())
        .with_state(HttpState {
            coordinator,
            token: Arc::new(token),
        })
}

async fn gitea_webhook(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if state.coordinator.manifest.gitea.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let header = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    };
    let event = headers
        .get("x-gitea-event-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| header("x-gitea-event"));
    match state
        .coordinator
        .gitea_webhook(
            header("x-gitea-signature"),
            event,
            header("x-gitea-delivery"),
            &body,
        )
        .await
    {
        Ok(ids) => Json(json!({"messages":ids})).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

#[allow(
    clippy::expect_used,
    reason = "HMAC-SHA256 accepts keys of every length"
)]
fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let Some(provided) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };
    // Compare fixed-length MACs using the cryptographic implementation's
    // constant-time verifier, without exposing either credential in diagnostics.
    let mut expected_mac =
        Hmac::<Sha256>::new_from_slice(expected.as_bytes()).expect("HMAC accepts any key length");
    expected_mac.update(b"codex-team-operator");
    let mut provided_mac =
        Hmac::<Sha256>::new_from_slice(provided.as_bytes()).expect("HMAC accepts any key length");
    provided_mac.update(b"codex-team-operator");
    provided_mac
        .verify_slice(&expected_mac.finalize().into_bytes())
        .is_ok()
}

async fn rpc(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<serde_json::Value>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let result = if request.get("operationId").is_some() {
        match serde_json::from_value(request) {
            Ok(operation) => state.coordinator.execute_operation(operation).await,
            Err(error) => Err(error.into()),
        }
    } else {
        match serde_json::from_value::<TeamRequest>(request) {
            Ok(request) => state.coordinator.execute(request).await,
            Err(error) => Err(error.into()),
        }
    };
    match result {
        Ok(response) => Json(response).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn events(
    State(state): State<HttpState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |mut socket| async move {
        let mut changed = state.coordinator.changed.subscribe();
        loop {
            let revision = *changed.borrow_and_update();
            if socket.send(Message::Text(json!({"revision": revision}).to_string().into())).await.is_err() { break; }
            tokio::select! {
                result = changed.changed() => if result.is_err() { break; },
                message = socket.recv() => match message {
                    Some(Ok(Message::Ping(bytes))) => if socket.send(Message::Pong(bytes)).await.is_err() { break; },
                    Some(Ok(Message::Pong(_))) => {},
                    _ => break,
                },
            }
        }
    }).into_response()
}

async fn relay(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path((machine, server)): Path<(String, String)>,
    ws: WebSocketUpgrade,
) -> Response {
    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let endpoint = match state.coordinator.endpoint(&machine, &server).await {
        Ok(endpoint) => endpoint,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    ws.max_message_size(128 << 20)
        .max_frame_size(128 << 20)
        .on_upgrade(move |socket| async move {
            if let Err(error) = endpoint.relay(socket).await {
                tracing::debug!(%error, "interactive team relay disconnected");
            }
        })
        .into_response()
}
