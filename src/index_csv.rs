use std::{error::Error, path::PathBuf};

use tantivy::Document;

use crate::{index_tantivy::FileSearchIndex, utils::convert_row_column_to_letter};

pub async fn index_csv_file(
    file_search_index: FileSearchIndex,
    path_to_csv: impl Into<PathBuf>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let path = path_to_csv.into();
    tracing::info!("indexing start for csv {path:?}.");
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false) // neededd
        .from_path(&path)?;
    let records: Vec<_> = rdr.records().filter_map(|r| r.ok()).collect();
    if records.len() < 2 {
        tracing::info!("not enough row to index...");
        return Ok(());
    }

    let labels: Vec<_> = records
        .iter()
        .take(1)
        .flat_map(|c| c.iter().map(|x| x.to_string()))
        .collect();
    let mut index_writer = file_search_index.index_writer.lock().await;

    for (row_idx, row) in records.iter().skip(1).enumerate() {
        if row.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        let mut doc = Document::default();
        doc.add_text(
            file_search_index.file_name_field,
            path.file_name().ok_or("file not found")?.to_string_lossy(),
        );
        doc.add_text(file_search_index.sheet_name_field, "Sheet1");
        for (column, cell) in row.iter().enumerate() {
            let cell = cell.trim();
            if cell.is_empty() {
                continue;
            }
            doc.add_text(
                file_search_index.cell_position_field,
                convert_row_column_to_letter(row_idx + 1, column), // we skip one row
            );
            doc.add_text(file_search_index.cell_ctx_field, &labels[column]);
            doc.add_text(file_search_index.cell_value_field, cell.to_string());
        }

        index_writer.add_document(doc)?;
    }
    index_writer.commit()?;
    tracing::info!("indexing done.");

    Ok(())
}
