use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::info;
use uuid::Uuid;

use crate::store::MemStore;

mod store;

#[derive(Parser)]
#[command(name = "demo-webapp")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:3000")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,demo_webapp=debug".into()),
        )
        .init();

    let args = Args::parse();
    let state = Arc::new(RwLock::new(MemStore::default()));

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/notes", post(create_note).get(list_notes))
        .route("/notes/{id}", get(get_note))
        .with_state(state);

    info!("listening on {}", args.listen);
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Note {
    id: Uuid,
    text: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

async fn create_note(
    State(s): State<Arc<RwLock<MemStore>>>,
    Json(input): Json<CreateNote>,
) -> impl IntoResponse {
    let note = Note {
        id: Uuid::new_v4(),
        text: input.text,
        created_at: chrono::Utc::now(),
    };
    s.write().await.insert(note.clone());
    (StatusCode::CREATED, Json(note))
}

async fn list_notes(State(s): State<Arc<RwLock<MemStore>>>) -> Json<Vec<Note>> {
    Json(s.read().await.list())
}

async fn get_note(
    State(s): State<Arc<RwLock<MemStore>>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Note>, StatusCode> {
    s.read().await.get(id).map(Json).ok_or(StatusCode::NOT_FOUND)
}

#[derive(Debug, Deserialize)]
struct CreateNote {
    text: String,
}
