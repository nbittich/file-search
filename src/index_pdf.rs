use std::{error::Error, path::Path, process::Command};

use crate::{
    index_tantivy::{DocMeta, FileSearchIndex},
    ocr,
    utils::{chunks, has_meaningful_text},
};

type ResultBox<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// The text layer of a pdf: poppler first, pdf-extract when poppler gives nothing. A defective PDF becomes an
/// ingestion error, never a Tokio worker panic.
fn extract_pdf_text(path: &Path) -> Result<String, String> {
    // `-` writes UTF-8 text to stdout; no temporary output in the read-only upload mount.
    let poppler_error = match Command::new("pdftotext")
        .args(["-enc", "UTF-8", "-layout"])
        .arg(path)
        .arg("-")
        .output()
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            if has_meaningful_text(&text) {
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
        pdf_extract::extract_text(path)
    }));
    match result {
        Ok(Ok(text)) if has_meaningful_text(&text) => Ok(text),
        Ok(Ok(_)) => Err(format!("{poppler_error}; pdf-extract returned no text")),
        Ok(Err(err)) => Err(format!("{poppler_error}; pdf-extract failed: {err}")),
        Err(_) => Err(format!(
            "{poppler_error}; pdf-extract panicked on malformed PDF"
        )),
    }
}

/// The text of a pdf: its text layer, or OCR of its pages (scanned receipts and invoices).
async fn pdf_text(path: &Path) -> ResultBox<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> ResultBox<String> {
        let text_layer_error = match extract_pdf_text(&path) {
            Ok(text) => return Ok(text),
            Err(e) => e,
        };
        tracing::info!("no text layer in {path:?} ({text_layer_error}), trying OCR");
        let text =
            ocr::ocr_pdf(&path).map_err(|e| format!("{text_layer_error}; OCR failed: {e}"))?;
        if !has_meaningful_text(&text) {
            return Err(format!("{text_layer_error}; OCR found no text").into());
        }
        Ok(text)
    })
    .await
    .map_err(|err| format!("PDF extraction worker failed: {err}"))?
}

pub async fn index_pdf_file(
    file_search_index: &FileSearchIndex,
    path: &Path,
    meta: &DocMeta,
) -> ResultBox<()> {
    // Extract outside the Tantivy lock so one bad document cannot block ingestion.
    let out = pdf_text(path).await?;
    let mut index_writer = file_search_index.index_writer.lock().await;
    index_writer.delete_term(tantivy::Term::from_field_text(
        file_search_index.source_path_field,
        &meta.source_path,
    ));
    for (chunk, text) in chunks(&out).into_iter().enumerate() {
        let mut doc = file_search_index.new_doc(meta);
        doc.add_text(
            file_search_index.cell_position_field,
            format!("chunk {}", chunk + 1),
        );
        doc.add_text(file_search_index.cell_value_field, text);
        index_writer.add_document(doc)?;
    }
    index_writer.commit()?;
    tracing::info!("indexing of {} done.", meta.source_path);

    Ok(())
}
