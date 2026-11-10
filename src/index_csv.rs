use std::{error::Error, io::Read, path::Path};

use crate::{
    index_tantivy::{DocMeta, FileSearchIndex},
    index_xlsx::add_row,
};

type ResultBox<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// The delimiter of a csv from its first lines: the candidate found the most often, the same number of times on
/// each line (`;` is common in Belgian/French exports). Tab for .tsv.
pub fn sniff_delimiter(sample: &str, extension: &str) -> u8 {
    if extension == "tsv" {
        return b'\t';
    }
    let lines: Vec<&str> = sample
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(10)
        .collect();
    let mut best = (b',', 0usize, false);
    for candidate in [b',', b';', b'\t', b'|'] {
        let counts: Vec<usize> = lines
            .iter()
            .map(|l| l.bytes().filter(|b| *b == candidate).count())
            .collect();
        let first = counts.first().copied().unwrap_or(0);
        let consistent = first > 0 && counts.iter().all(|c| *c == first);
        // a consistent count wins over an inconsistent one, then the highest count
        if (consistent, first) > (best.2, best.1) {
            best = (candidate, first, consistent);
        }
    }
    best.0
}

pub async fn index_csv_file(
    file_search_index: &FileSearchIndex,
    path: &Path,
    meta: &DocMeta,
) -> ResultBox<()> {
    tracing::info!("indexing start for csv {path:?}.");
    let mut sample = Vec::with_capacity(16 * 1024);
    std::fs::File::open(path)?
        .take(16 * 1024)
        .read_to_end(&mut sample)?;
    let delimiter = sniff_delimiter(&String::from_utf8_lossy(&sample), &meta.extension);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter)
        .from_path(path)?;
    // not every export is UTF-8: byte records, decoded leniently
    let records: Vec<Vec<String>> = rdr
        .byte_records()
        .filter_map(|r| r.ok())
        .map(|r| {
            r.iter()
                .map(|c| String::from_utf8_lossy(c).trim().to_string())
                .collect()
        })
        .collect();
    if records.is_empty() {
        return Err("empty csv".into());
    }
    let labels = records[0].clone();
    let mut index_writer = file_search_index.index_writer.lock().await;
    index_writer.delete_term(tantivy::Term::from_field_text(
        file_search_index.source_path_field,
        &meta.source_path,
    ));
    for (row_idx, row) in records.iter().enumerate() {
        if let Some(doc) = add_row(file_search_index, meta, "Sheet1", &labels, row_idx, row) {
            index_writer.add_document(doc)?;
        }
    }
    index_writer.commit()?;
    tracing::info!("indexing done.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::sniff_delimiter;

    #[test]
    fn delimiters() {
        assert_eq!(sniff_delimiter("a;b;c\n1;2;3\n", "csv"), b';');
        assert_eq!(sniff_delimiter("a,b,c\n1,2,3\n", "csv"), b',');
        // the comma of a decimal number doesn't fool it
        assert_eq!(sniff_delimiter("name;amount\nx;1,5\ny;12\n", "csv"), b';');
        assert_eq!(sniff_delimiter("a\tb\n1\t2\n", "csv"), b'\t');
        assert_eq!(sniff_delimiter("whatever", "tsv"), b'\t');
        assert_eq!(sniff_delimiter("single column\nvalue\n", "csv"), b',');
    }
}
