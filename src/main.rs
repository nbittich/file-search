#![allow(unused)]

use std::{env::var, error::Error, net::SocketAddr, path::PathBuf, str::FromStr};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use index_tantivy::QueryType;
use serde::{Deserialize, Serialize};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing::Level;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use crate::index_tantivy::FileSearchIndex;
mod extract_xlsx;
mod index_tantivy;
pub static CORS_ALLOW_ORIGIN: &str = "CORS_ALLOW_ORIGIN";
pub static BODY_SIZE_LIMIT: &str = "BODY_SIZE_LIMIT";
pub static SERVICE_HOST: &str = "SERVICE_HOST";
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
pub struct IndexRequest {
    file_path: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    setup_tracing()?;
    let host = var(SERVICE_HOST).unwrap_or_else(|_| String::from("0.0.0.0"));
    let port = var(SERVICE_PORT).unwrap_or_else(|_| String::from("8080"));
    let app_name = var(SERVICE_APPLICATION_NAME).unwrap_or_else(|_| String::from("file-search"));
    let file_search_index = FileSearchIndex::new(
        &var(INDEX_DIR_PATH).unwrap_or_else(|_| "/tmp/__tantivy_data".to_string()),
    )?;

    let addr = SocketAddr::from_str(&format!("{host}:{port}"))?;
    tracing::info!("{app_name} :: listening on {:?}", addr);
    let app = Router::new()
        .route("/index", post(post_index))
        .route("/search", get(get_search))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        )
        .with_state(file_search_index);
    axum::Server::bind(&addr)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await?;
    Ok(())
}
pub fn setup_tracing() -> Result<(), Box<dyn Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .with_env_filter(EnvFilter::from_default_env())
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;
    Ok(())
}

async fn get_search(
    query: Query<SearchRequest>,
    State(fsi): State<FileSearchIndex>,
) -> axum::response::Result<impl IntoResponse> {
    let docs = fsi
        .search(query.page, query.per_page, &query.q, &query.query_type)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(docs))
}

async fn post_index(
    State(fsi): State<FileSearchIndex>,
    axum::extract::Json(index_request): axum::extract::Json<IndexRequest>,
) -> axum::response::Result<impl IntoResponse> {
    let path = PathBuf::from(index_request.file_path);
    tracing::info!("getting path {path:?}");
    match path
        .extension()
        .and_then(|e| e.to_str())
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
    {
        "xls" | "xlsx" if path.exists() => {
            tokio::spawn(async move { extract_xlsx::index_xlsx_file(fsi, path) });
            Ok(StatusCode::ACCEPTED)
        }
        _ => {
            tracing::error!("{path:?} not yet supported");
            Ok(StatusCode::FORBIDDEN)
        }
    }
}
