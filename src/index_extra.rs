use crate::index_tantivy::FileSearchIndex;
use std::{error::Error, path::PathBuf, process::Command};
use tantivy::TantivyDocument;

pub async fn index_extra(
    index: &FileSearchIndex,
    path: PathBuf,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let content = match ext.as_str() {
        "txt" | "md" | "json" | "xml" | "log" => std::fs::read_to_string(&path)?,
        "png" | "jpg" | "jpeg" | "tif" | "tiff" => {
            let output = Command::new("tesseract")
                .arg(&path)
                .arg("stdout")
                .output()?;
            if !output.status.success() {
                return Err("OCR failed".into());
            }
            String::from_utf8(output.stdout)?
        }
        "docx" | "odt" | "doc" => {
            let dir =
                std::env::temp_dir().join(format!("file-search-convert-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir)?;
            let result = (|| -> Result<String, Box<dyn Error + Send + Sync>> {
                let output = Command::new("libreoffice")
                    .arg("-env:UserInstallation=file:///tmp/lo-file-search")
                    .args(["--headless", "--convert-to", "txt:Text", "--outdir"])
                    .arg(&dir)
                    .arg(&path)
                    .output()?;
                if !output.status.success() {
                    return Err("document conversion failed".into());
                }
                let text = std::fs::read_to_string(
                    dir.join(path.file_stem().ok_or("invalid file")?)
                        .with_extension("txt"),
                )?;
                Ok(text)
            })();
            let _ = std::fs::remove_dir_all(&dir);
            result?
        }
        _ => return Err("unsupported file".into()),
    };
    if content.trim().is_empty() {
        return Err("no extractable text".into());
    }
    let words: Vec<&str> = content.split_whitespace().collect();
    let mut writer = index.index_writer.lock().await;
    writer.delete_term(tantivy::Term::from_field_text(
        index.source_path_field,
        &path.to_string_lossy(),
    ));
    for (n, start) in (0..words.len()).step_by(180).enumerate() {
        let mut doc = TantivyDocument::default();
        doc.add_text(
            index.cell_value_field,
            words[start..(start + 220).min(words.len())].join(" "),
        );
        doc.add_text(index.cell_position_field, format!("chunk {}", n + 1));
        doc.add_text(
            index.file_name_field,
            path.file_name().unwrap_or_default().to_string_lossy(),
        );
        doc.add_text(index.source_path_field, path.to_string_lossy());
        writer.add_document(doc)?;
    }
    writer.commit()?;
    Ok(())
}
