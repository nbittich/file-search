use std::{env::var, error::Error, net::SocketAddr, path::PathBuf, str::FromStr};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use chrono::Local;
use index_tantivy::QueryType;
use serde::Deserialize;
use time::{UtcOffset, macros::format_description};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing::Level;
use tracing_subscriber::{EnvFilter, FmtSubscriber, fmt::time::OffsetTime};

use crate::index_tantivy::FileSearchIndex;
mod index_csv;
mod index_extra;
mod index_pdf;
mod index_tantivy;
mod index_xlsx;
mod ingest;
mod utils;

pub static CORS_ALLOW_ORIGIN: &str = "CORS_ALLOW_ORIGIN";
pub static BODY_SIZE_LIMIT: &str = "BODY_SIZE_LIMIT";
pub static SERVICE_HOST: &str = "SERVICE_HOST";
pub static INDEX_WRITER_SIZE: &str = "INDEX_WRITER_SIZE";
pub static SERVICE_PORT: &str = "SERVICE_PORT";
pub static SERVICE_CONFIG_VOLUME: &str = "SERVICE_CONFIG_VOLUME";
pub static SERVICE_DATA_VOLUME: &str = "SERVICE_DATA_VOLUME";
pub static SERVICE_APPLICATION_NAME: &str = "SERVICE_APPLICATION_NAME";
pub static SERVICE_COLLECTION_NAME: &str = "SERVICE_COLLECTION_NAME";
pub static INDEX_DIR_PATH: &str = "INDEX_DIR_PATH";

#[derive(Deserialize)]
pub struct SearchRequest {
    page: usize,
    per_page: usize,
    q: String,
    query_type: QueryType,
}
#[derive(Deserialize)]
pub struct FindAllRequest {
    page: usize,
    per_page: usize,
}
#[derive(Deserialize)]
pub struct IndexRequest {
    file_path: String,
}

#[derive(Deserialize)]
pub struct ReindexRequest {
    directory_path: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    setup_tracing()?;
    let host = var(SERVICE_HOST).unwrap_or_else(|_| String::from("0.0.0.0"));
    let index_writer_size = var(INDEX_WRITER_SIZE)
        .unwrap_or_else(|_| String::from("50000000"))
        .parse::<usize>()?;
    let port = var(SERVICE_PORT).unwrap_or_else(|_| String::from("8080"));
    let app_name = var(SERVICE_APPLICATION_NAME).unwrap_or_else(|_| String::from("file-search"));
    let file_search_index = FileSearchIndex::new(
        &var(INDEX_DIR_PATH).unwrap_or_else(|_| "/tmp/__tantivy_data".to_string()),
        index_writer_size,
    )?;

    let upload_root = var("UPLOAD_DIR").unwrap_or_else(|_| "/tmp/uploads".to_string());
    let state_dir = var("INGEST_STATE_DIR").unwrap_or_else(|_| "/tmp/app/state".to_string());
    let ingestor = ingest::Ingestor::new(
        file_search_index.clone(),
        PathBuf::from(upload_root),
        PathBuf::from(state_dir),
        &PathBuf::from(var(INDEX_DIR_PATH).unwrap_or_else(|_| "/tmp/__tantivy_data".into())),
    )
    .map_err(|e| format!("{e}"))?;
    let shared = ingestor.clone();
    ingestor.start();
    let addr = SocketAddr::from_str(&format!("{host}:{port}"))?;
    tracing::info!("{app_name} :: listening on {:?}", addr);
    let app = Router::new()
        .route("/index", post(post_index))
        .route("/reindex", post(reindex_from_directory))
        .route("/search", get(get_search))
        .route("/find-all", get(get_find_all))
        .route("/stats", get(get_stats))
        .route("/reset", post(reset_index))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        )
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind(addr).await?;

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
pub fn setup_tracing() -> Result<(), Box<dyn Error>> {
    let offset_hours = {
        let now = Local::now();
        let offset_seconds = now.offset().local_minus_utc();
        let hours = offset_seconds / 3600;
        hours as i8
    };
    let offset = UtcOffset::from_hms(offset_hours, 0, 0)?;

    let timer = OffsetTime::new(
        offset,
        format_description!("[day]-[month]-[year] [hour]:[minute]:[second]"),
    );
    let subscriber = FmtSubscriber::builder()
        .with_timer(timer)
        .with_max_level(Level::TRACE)
        .with_env_filter(EnvFilter::from_default_env())
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;
    Ok(())
}

async fn get_search(
    query: Query<SearchRequest>,
    State(ingestor): State<ingest::Ingestor>,
) -> Result<impl IntoResponse, StatusCode> {
    let docs = ingestor
        .index
        .search(query.page, query.per_page, &query.q, &query.query_type)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(docs))
}
async fn get_find_all(
    query: Query<FindAllRequest>,
    State(ingestor): State<ingest::Ingestor>,
) -> Result<impl IntoResponse, StatusCode> {
    let docs = ingestor
        .index
        .find_all(query.page, query.per_page)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(docs))
}
async fn post_index(
    State(ingestor): State<ingest::Ingestor>,
    axum::extract::Json(index_request): axum::extract::Json<IndexRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let path = PathBuf::from(index_request.file_path);
    tracing::info!("getting path {path:?}");
    index_path(path, ingestor).await
}

async fn get_stats(State(ingestor): State<ingest::Ingestor>) -> Json<ingest::Stats> {
    Json(ingestor.stats().await)
}
async fn reset_index(State(ingestor): State<ingest::Ingestor>) -> Result<StatusCode, StatusCode> {
    ingestor.reset().await.map_err(|e| {
        tracing::error!("reset: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    // A new scan will index every upload from scratch.
    Ok(StatusCode::ACCEPTED)
}
async fn reindex_from_directory(
    State(ingestor): State<ingest::Ingestor>,
    reindex: Query<ReindexRequest>,
) -> Result<StatusCode, StatusCode> {
    let dir = PathBuf::from(&reindex.directory_path);
    if !dir.is_dir() {
        return Err(StatusCode::BAD_REQUEST);
    }
    ingestor
        .reset()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // The upload watcher is responsible for upload folder contents.
    ingestor
        .scan()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::ACCEPTED)
}
async fn index_path(path: PathBuf, ingestor: ingest::Ingestor) -> Result<StatusCode, StatusCode> {
    if !path.is_file() {
        return Err(StatusCode::BAD_REQUEST);
    }
    ingestor.process(path).await.map_err(|e| {
        tracing::error!("index failed: {e}");
        StatusCode::UNPROCESSABLE_ENTITY
    })?;
    Ok(StatusCode::ACCEPTED)
}
