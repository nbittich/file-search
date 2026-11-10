use std::{error::Error, path::Path};

use calamine::{Data, DataType, Reader, open_workbook_auto};
use tantivy::TantivyDocument;

use crate::{
    index_tantivy::{DocMeta, FileSearchIndex},
    utils::convert_row_column_to_letter,
};

type ResultBox<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// The text of a cell: dates as dd/mm/yyyy (not the excel serial number), integers without ".0".
pub fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
        Data::DateTime(d) if d.is_duration() => cell.to_string(),
        Data::DateTime(_) | Data::DateTimeIso(_) => match cell.as_datetime() {
            Some(dt) if dt.time() == chrono::NaiveTime::MIN => dt.format("%d/%m/%Y").to_string(),
            Some(dt) => dt.format("%d/%m/%Y %H:%M").to_string(),
            None => cell.to_string(),
        },
        _ => cell.to_string(),
    }
    .trim()
    .to_string()
}

/// The document of a row: its cells "label: value" (the label of the column from the first row; the first row
/// itself as is), positions and labels. `None` for an empty row.
pub fn add_row(
    index: &FileSearchIndex,
    meta: &DocMeta,
    sheet_name: &str,
    labels: &[String],
    row_idx: usize,
    cells: &[String],
) -> Option<TantivyDocument> {
    if cells.iter().all(|c| c.is_empty()) {
        return None;
    }
    let mut doc = index.new_doc(meta);
    doc.add_text(index.sheet_name_field, sheet_name);
    for (column, cell) in cells.iter().enumerate() {
        if cell.is_empty() {
            continue;
        }
        doc.add_text(
            index.cell_position_field,
            convert_row_column_to_letter(row_idx, column),
        );
        let label = labels.get(column).filter(|l| !l.is_empty() && row_idx > 0);
        match label {
            Some(label) => {
                doc.add_text(index.cell_ctx_field, label);
                doc.add_text(index.cell_value_field, format!("{label}: {cell}"));
            }
            None => doc.add_text(index.cell_value_field, cell),
        }
    }
    Some(doc)
}

pub async fn index_xlsx_file(
    file_search_index: &FileSearchIndex,
    path: &Path,
    meta: &DocMeta,
) -> ResultBox<()> {
    let owned = path.to_path_buf();
    // a big workbook takes a while to read: not on an async worker
    let sheets =
        tokio::task::spawn_blocking(move || -> ResultBox<Vec<(String, Vec<Vec<String>>)>> {
            let mut workbook = open_workbook_auto(&owned)?;
            let mut sheets = Vec::new();
            for sheet_name in workbook.sheet_names().to_owned() {
                match workbook.worksheet_range(&sheet_name) {
                    Ok(range) => sheets.push((
                        sheet_name,
                        range
                            .rows()
                            .map(|row| row.iter().map(cell_text).collect())
                            .collect(),
                    )),
                    Err(e) => tracing::warn!("sheet {sheet_name} of {owned:?} not readable: {e}"),
                }
            }
            Ok(sheets)
        })
        .await
        .map_err(|e| format!("workbook worker failed: {e}"))??;

    let mut index_writer = file_search_index.index_writer.lock().await;
    index_writer.delete_term(tantivy::Term::from_field_text(
        file_search_index.source_path_field,
        &meta.source_path,
    ));
    for (sheet_name, rows) in sheets {
        let Some(labels) = rows.first().cloned() else {
            continue;
        };
        for (row_idx, row) in rows.iter().enumerate() {
            if let Some(doc) = add_row(file_search_index, meta, &sheet_name, &labels, row_idx, row)
            {
                index_writer.add_document(doc)?;
            }
        }
    }
    index_writer.commit()?;
    tracing::info!("indexing of {} done.", meta.source_path);

    Ok(())
}

#[cfg(test)]
mod test {
    use calamine::{Data, ExcelDateTime, ExcelDateTimeType};
    use tantivy::schema::Value;

    use super::{add_row, cell_text};
    use crate::index_tantivy::{DocMeta, test_support::temp_index};

    #[test]
    fn cells() {
        assert_eq!(cell_text(&Data::Float(1200.0)), "1200");
        assert_eq!(cell_text(&Data::Float(12.5)), "12.5");
        assert_eq!(cell_text(&Data::String(" ACME ".into())), "ACME");
        assert_eq!(cell_text(&Data::Empty), "");
        // 46000 = 2025-12-09 in excel
        let date = Data::DateTime(ExcelDateTime::new(
            46000.0,
            ExcelDateTimeType::DateTime,
            false,
        ));
        assert_eq!(cell_text(&date), "09/12/2025");
        assert_eq!(
            cell_text(&Data::DateTimeIso("2026-10-05T14:30:00".into())),
            "05/10/2026 14:30"
        );
    }

    #[test]
    fn rows_have_labels() {
        let index = temp_index();
        let meta = DocMeta {
            source_path: "/uploads/a.xlsx".into(),
            origin_path: "/uploads/a.xlsx".into(),
            file_name: "a.xlsx".into(),
            kind: "spreadsheet",
            extension: "xlsx".into(),
            modified_at: None,
        };
        let labels = vec!["Client".to_string(), "Amount".to_string()];
        let header = add_row(&index, &meta, "S1", &labels, 0, &labels).unwrap();
        let values: Vec<&str> = header
            .get_all(index.cell_value_field)
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(values, vec!["Client", "Amount"]);

        let row = add_row(&index, &meta, "S1", &labels, 1, &["ACME".into(), "".into()]).unwrap();
        let values: Vec<&str> = row
            .get_all(index.cell_value_field)
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(values, vec!["Client: ACME"]);
        let positions: Vec<&str> = row
            .get_all(index.cell_position_field)
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(positions, vec!["A2"]);
        assert!(add_row(&index, &meta, "S1", &labels, 2, &["".into(), "".into()]).is_none());
    }
}
