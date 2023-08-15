use std::{error::Error, path::PathBuf};

use regex::Regex;
use tantivy::Document;

use crate::index_tantivy::FileSearchIndex;

pub async fn index_pdf_file(
    file_search_index: FileSearchIndex,
    path: impl Into<PathBuf>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let path = path.into();
    let out = pdf_extract::extract_text(&path)?;
    let regex = Regex::new("((?:[^\n][\n]?)+)")?;

    let mut index_writer = file_search_index.index_writer.lock().await;

    tracing::info!("indexing start for pdf {path:?}.");
    for (row, line) in regex.split(&out).enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut doc = Document::default();
        doc.add_text(
            file_search_index.cell_position_field,
            &format!("line {}", row + 1),
        );
        doc.add_text(file_search_index.cell_value_field, line);
        doc.add_text(
            file_search_index.file_name_field,
            path.file_name().ok_or("file not found")?.to_string_lossy(),
        );

        index_writer.add_document(doc)?;
    }
    index_writer.commit()?;
    tracing::info!("indexing done.");

    Ok(())
}

#[cfg(test)]
mod test {
    use crate::index_tantivy::FileSearchIndex;

    use super::index_pdf_file;
    #[tokio::test]
    //#[ignore]
    async fn test_pdf() {
        let file_search_index = FileSearchIndex::new(
            &std::env::var("INDEX_DIR_PATH").unwrap_or_else(|_| "/tmp/__tantivy_data".to_string()),
            50_000_000,
        )
        .unwrap();
        index_pdf_file(file_search_index, "test3.pdf")
            .await
            .unwrap();
    }
}
