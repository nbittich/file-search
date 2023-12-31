use std::{env::var, error::Error, net::SocketAddr, path::PathBuf, str::FromStr};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{ErrorResponse, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use chrono::Local;
use index_tantivy::QueryType;
use serde::Deserialize;
use time::{macros::format_description, UtcOffset};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing::Level;
use tracing_subscriber::{fmt::time::OffsetTime, EnvFilter, FmtSubscriber};

use crate::index_tantivy::FileSearchIndex;
mod index_csv;
mod index_pdf;
mod index_tantivy;
mod index_xlsx;
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

    let addr = SocketAddr::from_str(&format!("{host}:{port}"))?;
    tracing::info!("{app_name} :: listening on {:?}", addr);
    let app = Router::new()
        .route("/index", post(post_index))
        .route("/reindex", post(reindex_from_directory))
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
    index_path(path, fsi).await
}

async fn reindex_from_directory(
    State(fsi): State<FileSearchIndex>,
    reindex: Query<ReindexRequest>,
) -> axum::response::Result<impl IntoResponse> {
    let path = PathBuf::from(&reindex.directory_path);
    if !path.exists() || !path.is_dir() {
        tracing::error!("path {path:?} doesn't exist or is not a directory");
        return Err(ErrorResponse::from(StatusCode::BAD_REQUEST));
    }

    let mut index_writer = fsi.index_writer.lock().await;

    index_writer.delete_all_documents().map_err(|e| {
        tracing::error!("error while deleting all docs: {e:?}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    index_writer.commit().map_err(|e| {
        tracing::error!("error while committing: {e:?}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    for file in path.read_dir().map_err(|e| {
        tracing::error!("could not read dir {e:?}");
        ErrorResponse::from(StatusCode::INTERNAL_SERVER_ERROR)
    })? {
        let file = file.map_err(|e| {
            tracing::error!("could not read file {e:?}");
            ErrorResponse::from(StatusCode::INTERNAL_SERVER_ERROR)
        })?;
        index_path(file.path(), fsi.clone()).await?;
    }

    Ok(StatusCode::ACCEPTED)
}

async fn index_path(
    path: PathBuf,
    fsi: FileSearchIndex,
) -> axum::response::Result<impl IntoResponse> {
    match path.extension().and_then(|e| e.to_str()).ok_or_else(|| {
        tracing::error!("could not determine extension");
        StatusCode::INTERNAL_SERVER_ERROR
    })? {
        "xls" | "xlsx" if path.exists() => {
            tokio::spawn(async move { index_xlsx::index_xlsx_file(fsi, path).await });
            Ok(StatusCode::ACCEPTED)
        }
        "csv" if path.exists() => {
            tokio::spawn(async move { index_csv::index_csv_file(fsi, path).await });
            Ok(StatusCode::ACCEPTED)
        }
        "pdf" if path.exists() => {
            tokio::spawn(async move { index_pdf::index_pdf_file(fsi, path).await });
            Ok(StatusCode::ACCEPTED)
        }
        _ => {
            tracing::error!("{path:?} not yet supported");
            Err(ErrorResponse::from(StatusCode::FORBIDDEN))
        }
    }
}
