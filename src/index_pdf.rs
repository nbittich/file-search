use std::{
    error::Error,
    path::{Path, PathBuf},
    process::Command,
};

use tantivy::TantivyDocument;

use crate::index_tantivy::FileSearchIndex;

/// A defective PDF must become an ingestion error, never a Tokio worker panic.
async fn extract_pdf_text(path: &Path) -> Result<String, Box<dyn Error + Send + Sync>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<String, String> {
        // `-` writes UTF-8 text to stdout; no temporary output in the read-only upload mount.
        let poppler_error = match Command::new("pdftotext")
            .args(["-enc", "UTF-8", "-layout"])
            .arg(&path)
            .arg("-")
            .output()
        {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout).into_owned();
                if !text.trim().is_empty() {
                    return Ok(text);
                }
                "pdftotext returned no text".to_owned()
            }
            Ok(output) => format!(
                "pdftotext failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(err) => format!("pdftotext unavailable: {err}"),
        };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pdf_extract::extract_text(&path)
        }));
        match result {
            Ok(Ok(text)) if !text.trim().is_empty() => Ok(text),
            Ok(Ok(_)) => Err(format!("{poppler_error}; pdf-extract returned no text")),
            Ok(Err(err)) => Err(format!("{poppler_error}; pdf-extract failed: {err}")),
            Err(_) => Err(format!(
                "{poppler_error}; pdf-extract panicked on malformed PDF"
            )),
        }
    })
    .await
    .map_err(|err| format!("PDF extraction worker failed: {err}"))?
    .map_err(|err| err.into())
}

pub async fn index_pdf_file(
    file_search_index: &FileSearchIndex,
    path: impl Into<PathBuf>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let path = path.into();
    // Prefer Poppler, which handles many malformed PDFs that panic in pdf-extract.
    // Extract outside the Tantivy lock so one bad document cannot block ingestion.
    let out = extract_pdf_text(&path).await?;
    if out.trim().is_empty() {
        return Err("PDF has no extractable text (image-only PDFs require OCR)".into());
    }
    let mut index_writer = file_search_index.index_writer.lock().await;
    index_writer.delete_term(tantivy::Term::from_field_text(
        file_search_index.source_path_field,
        &path.to_string_lossy(),
    ));
    // Paragraph-sized, overlapping chunks retain context without creating huge documents.
    let words: Vec<&str> = out.split_whitespace().collect();
    for (chunk, start) in (0..words.len()).step_by(180).enumerate() {
        let end = (start + 220).min(words.len());
        let text = words[start..end].join(" ");
        if text.trim().is_empty() {
            continue;
        }
        let mut doc = TantivyDocument::default();
        doc.add_text(
            file_search_index.cell_position_field,
            format!("chunk {}", chunk + 1),
        );
        doc.add_text(file_search_index.cell_value_field, text);
        doc.add_text(file_search_index.source_path_field, path.to_string_lossy());
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
    async fn test_pdf() {
        let file_search_index = FileSearchIndex::new(
            &std::env::var("INDEX_DIR_PATH").unwrap_or_else(|_| "/tmp/__tantivy_data".to_string()),
            50_000_000,
        )
        .unwrap();
        index_pdf_file(&file_search_index, "BXLCityReportDraft3.pdf")
            .await
            .unwrap();

        let res = file_search_index.find_all(0, 10).unwrap();
        dbg!(res);
    }

    #[tokio::test]
    #[ignore]
    async fn test_buggy_pdf() {
        // https://github.com/jrmuizel/pdf-extract/issues/67
        let out = pdf_extract::extract_text("dummycv.pdf").unwrap();
        println!("{out}");
    }
}
