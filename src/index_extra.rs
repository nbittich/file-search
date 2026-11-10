use crate::{
    index_tantivy::{DocMeta, FileSearchIndex},
    ocr,
    utils::{chunks, html_to_text},
};
use std::{
    error::Error,
    path::{Path, PathBuf},
    process::Command,
};

type ResultBox<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// Text of a word processor document (doc, docx, odt, rtf) converted by LibreOffice.
fn document_text(path: &Path) -> ResultBox<String> {
    let dir = std::env::temp_dir().join(format!("file-search-convert-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    let result = (|| -> ResultBox<String> {
        let output = Command::new("libreoffice")
            .arg("-env:UserInstallation=file:///tmp/lo-file-search")
            .args(["--headless", "--convert-to", "txt:Text", "--outdir"])
            .arg(&dir)
            .arg(path)
            .output()?;
        if !output.status.success() {
            return Err("document conversion failed".into());
        }
        let bytes = std::fs::read(
            dir.join(path.file_stem().ok_or("invalid file")?)
                .with_extension("txt"),
        )?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Text of a text, image or document file.
fn extract(path: &Path, extension: &str) -> ResultBox<String> {
    Ok(match extension {
        // not all text files are UTF-8 (latin-1 exports...): replaced characters rather than a failure
        "txt" | "md" | "json" | "xml" | "log" | "eml" | "ics" | "yaml" | "yml" => {
            String::from_utf8_lossy(&std::fs::read(path)?).into_owned()
        }
        "html" | "htm" => html_to_text(&String::from_utf8_lossy(&std::fs::read(path)?)),
        "png" | "jpg" | "jpeg" | "tif" | "tiff" | "bmp" | "webp" => ocr::ocr_image(path)?,
        "docx" | "odt" | "doc" | "rtf" => document_text(path)?,
        _ => return Err("unsupported file".into()),
    })
}

pub async fn index_extra(index: &FileSearchIndex, path: &Path, meta: &DocMeta) -> ResultBox<()> {
    let extension = meta.extension.clone();
    let owned: PathBuf = path.to_path_buf();
    // OCR and conversions take seconds: not on an async worker
    let content = tokio::task::spawn_blocking(move || extract(&owned, &extension))
        .await
        .map_err(|e| format!("extraction worker failed: {e}"))??;
    if content.trim().is_empty() {
        return Err("no extractable text".into());
    }
    let mut writer = index.index_writer.lock().await;
    writer.delete_term(tantivy::Term::from_field_text(
        index.source_path_field,
        &meta.source_path,
    ));
    for (n, text) in chunks(&content).into_iter().enumerate() {
        let mut doc = index.new_doc(meta);
        doc.add_text(index.cell_value_field, text);
        doc.add_text(index.cell_position_field, format!("chunk {}", n + 1));
        writer.add_document(doc)?;
    }
    writer.commit()?;
    Ok(())
}
