use axum::{
    extract::State,
    http::StatusCode,
    Json,
    response::IntoResponse,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::io::Write;
use std::sync::atomic::Ordering;

use super::GuiHandles;

pub async fn get_config(State(handles): State<Arc<GuiHandles>>) -> impl IntoResponse {
    let contents = match std::fs::read_to_string(&handles.config_path) {
        Ok(s) => s,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("read error: {}", e))
                .into_response()
        }
    };
    let mut cfg: Value = match toml::from_str(&contents) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("parse error: {}", e))
                .into_response()
        }
    };
    if let Some(obj) = cfg.as_object_mut() {
        // No hot-reloadable fields on stream-playback side
        obj.insert(
            "_meta".to_string(),
            json!({ "hot_reloadable_fields": [] }),
        );
    }
    Json(cfg).into_response()
}

pub async fn post_config(
    State(handles): State<Arc<GuiHandles>>,
    Json(new_val): Json<Value>,
) -> impl IntoResponse {
    // Strip _meta
    let mut clean = new_val.clone();
    if let Some(obj) = clean.as_object_mut() {
        obj.remove("_meta");
    }

    let toml_string = match toml::to_string_pretty(&clean) {
        Ok(s) => s,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("serialize error: {}", e))
                .into_response()
        }
    };

    // Atomic write
    let dir = handles
        .config_path
        .parent()
        .unwrap_or(std::path::Path::new("."));
    let mut tmp = match tempfile::NamedTempFile::new_in(dir) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("tempfile error: {}", e),
            )
                .into_response()
        }
    };
    if let Err(e) = tmp.write_all(toml_string.as_bytes()) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("write error: {}", e))
            .into_response();
    }
    if let Err(e) = tmp.persist(&handles.config_path) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist error: {}", e),
        )
            .into_response();
    }

    // All fields on stream-playback require restart
    Json(json!({ "status": "saved", "restart_required": true })).into_response()
}

pub async fn post_restart() -> impl IntoResponse {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::process::exit(0);
    });
    Json(json!({ "status": "restarting" }))
}

pub async fn get_stats(State(handles): State<Arc<GuiHandles>>) -> impl IntoResponse {
    let (received, lost, pkt_id) = handles.stats.snapshot();
    let produced = handles.samples_produced.load(Ordering::Relaxed);
    let played = handles.playback_pos.load(Ordering::Relaxed);
    Json(json!({
        "received": received,
        "lost": lost,
        "pkt_id": pkt_id,
        "samples_produced": produced,
        "samples_played": played,
        "ring_occupied": produced.saturating_sub(played),
        "ring_capacity": handles.ring_capacity_samples,
    }))
}
