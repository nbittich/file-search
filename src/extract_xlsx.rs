use std::{error::Error, path::PathBuf};

use calamine::{open_workbook_auto, DataType, Reader};
use tantivy::Document;

use crate::index_tantivy::FileSearchIndex;

type Column = usize;
type Row = usize;
type Val = DataType;

fn convert_row_column_to_letter(mut row: Row, mut column: Column) -> String {
    row += 1;
    column += 1;
    let letters = ('A'..='Z').collect::<Vec<char>>();

    let mut res = String::new();
    if column < 26 {
        res.push(letters[column - 1]);
        res += &row.to_string();
        return res;
    }

    while column / 26 != 0 {
        let letter = column / 26;
        res.push(letters[letter - 1]);
        column %= 26;
    }

    if column > 0 {
        res.push(letters[column - 1]);
    }

    res += &row.to_string();

    res
}

pub fn index_xlsx_file(
    file_search_index: &mut FileSearchIndex,
    path_to_xlsx: &str,
) -> Result<(), Box<dyn Error>> {
    let path_to_xlsx = PathBuf::from(path_to_xlsx);
    let mut workbook = open_workbook_auto(&path_to_xlsx).expect("Cannot open file");
    let sheets = workbook.sheet_names().to_owned();

    for sheet_name in sheets {
        if let Some(Ok(range)) = workbook.worksheet_range(&sheet_name) {
            // extract labels
            let labels = range
                .rows()
                .take(1)
                .flat_map(|r| r.iter())
                .map(|c| c.to_string())
                .collect::<Vec<String>>();

            for (row_idx, row) in range.rows().skip(1).enumerate() {
                let mut doc = Document::default();
                doc.add_text(
                    file_search_index.file_name_field,
                    path_to_xlsx
                        .file_name()
                        .ok_or("file not found")?
                        .to_string_lossy(),
                );

                if row.iter().all(|c| c == &DataType::Empty) {
                    continue;
                }
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

                file_search_index.index_writer.add_document(doc)?;
            }
            file_search_index.index_writer.commit()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use tantivy::{
        collector::TopDocs,
        query::{QueryParser, RegexQuery},
    };

    use crate::{extract_xlsx::convert_row_column_to_letter, index_tantivy::FileSearchIndex};

    use super::index_xlsx_file;

    #[test]
    fn test_xlsx() {
        let mut file_search_index = FileSearchIndex::new(
            &std::env::var("INDEX_DIR_PATH").unwrap_or_else(|_| "/tmp/__tantivy_data".to_string()),
        )
        .unwrap();
        index_xlsx_file(&mut file_search_index, "test2.xlsx").unwrap();
    }

    #[test]
    fn test_read_idx() {
        let file_search_index = FileSearchIndex::new(
            &std::env::var("INDEX_DIR_PATH").unwrap_or_else(|_| "/tmp/__tantivy_data".to_string()),
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
    fn test_convert_row_column_to_letter() {
        assert_eq!(convert_row_column_to_letter(61, 8), "I62".to_string());
        assert_eq!(convert_row_column_to_letter(56, 132), "EC57".to_string());
        assert_eq!(convert_row_column_to_letter(14, 91), "CN15".to_string());
    }
}
