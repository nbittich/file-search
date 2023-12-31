use std::{error::Error, path::PathBuf};

use calamine::{open_workbook_auto, DataType, Reader};
use tantivy::Document;

use crate::{index_tantivy::FileSearchIndex, utils::convert_row_column_to_letter};

pub async fn index_xlsx_file(
    file_search_index: FileSearchIndex,
    path_to_xlsx: impl Into<PathBuf>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let path_to_xlsx = path_to_xlsx.into();
    let mut workbook = open_workbook_auto(&path_to_xlsx)?;
    let sheets = workbook.sheet_names().to_owned();

    let mut index_writer = file_search_index.index_writer.lock().await;
    for sheet_name in sheets {
        if let Some(Ok(range)) = workbook.worksheet_range(&sheet_name) {
            tracing::info!("indexing start for sheet {sheet_name}.");
            if range.rows().len() < 2 {
                tracing::info!("not enough row to index...");
                continue;
            }
            // extract labels
            let labels = range
                .rows()
                .take(1)
                .flat_map(|r| r.iter())
                .map(|c| c.to_string())
                .collect::<Vec<String>>();

            for (row_idx, row) in range.rows().skip(1).enumerate() {
                if row.iter().all(|c| c == &DataType::Empty) {
                    continue;
                }
                let mut doc = Document::default();
                doc.add_text(
                    file_search_index.file_name_field,
                    path_to_xlsx
                        .file_name()
                        .ok_or("file not found")?
                        .to_string_lossy(),
                );
                doc.add_text(file_search_index.sheet_name_field, &sheet_name);
                for (column, cell) in row.iter().enumerate() {
                    if &DataType::Empty == cell {
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
            tracing::info!("indexing done.");
        }
    }
    index_writer.commit()?;

    Ok(())
}

#[cfg(test)]
mod test {
    use tantivy::{
        collector::TopDocs,
        query::{QueryParser, RegexQuery},
    };

    use crate::{index_tantivy::FileSearchIndex, index_xlsx::convert_row_column_to_letter};

    use super::index_xlsx_file;

    #[tokio::test]
    #[ignore]
    async fn test_xlsx() {
        let file_search_index = FileSearchIndex::new(
            &std::env::var("INDEX_DIR_PATH").unwrap_or_else(|_| "/tmp/__tantivy_data".to_string()),
            50_000_000,
        )
        .unwrap();
        index_xlsx_file(file_search_index, "test2.xlsx")
            .await
            .unwrap();
    }
    #[test]
    #[ignore]
    fn test_read_idx() {
        let file_search_index = FileSearchIndex::new(
            &std::env::var("INDEX_DIR_PATH").unwrap_or_else(|_| "/tmp/__tantivy_data".to_string()),
            50_000_000,
        )
        .unwrap();

        let searcher = file_search_index.index_reader.searcher();
        println!("num docs: {}", searcher.num_docs());
        let schema = file_search_index.schema;

        let regex_query =
            RegexQuery::from_pattern("(?i)sango.*", file_search_index.cell_value_field).unwrap();
        let top_docs = searcher
            .search(&regex_query, &TopDocs::with_limit(10))
            .unwrap();
        assert_eq!(2, top_docs.len());
        for (_score, doc_address) in top_docs {
            let retrieved_doc = searcher.doc(doc_address).unwrap();
            println!("{}", schema.to_json(&retrieved_doc))
        }

        let query_parser = QueryParser::for_index(
            &file_search_index.index,
            vec![file_search_index.cell_value_field],
        );
        let query = query_parser
            .parse_query("\"Regionally membership\"~1")
            .unwrap();
        let top_docs = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
        for (_score, doc_address) in top_docs.iter() {
            let retrieved_doc = searcher.doc(*doc_address).unwrap();
            println!("{}", schema.to_json(&retrieved_doc))
        }
        assert_eq!(1, top_docs.len());

        assert!(searcher.num_docs() > 0);
    }

    #[test]
    #[ignore]
    fn test_convert_row_column_to_letter() {
        assert_eq!(convert_row_column_to_letter(61, 8), "I62".to_string());
        assert_eq!(convert_row_column_to_letter(56, 132), "EC57".to_string());
        assert_eq!(convert_row_column_to_letter(14, 91), "CN15".to_string());
    }
}
