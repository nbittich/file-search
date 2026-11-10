//! Text recognition with tesseract: images, and pdfs without a text layer (scanned receipts and invoices), whose
//! pages are rendered with pdftoppm (poppler) first.

use std::{
    env::var,
    error::Error,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

type ResultBox<T> = Result<T, Box<dyn Error + Send + Sync>>;

pub static OCR_LANGUAGES: &str = "OCR_LANGUAGES";
pub static OCR_MAX_PDF_PAGES: &str = "OCR_MAX_PDF_PAGES";

/// Languages wanted (OCR_LANGUAGES, default `eng+fra+nld+deu`) that tesseract actually has, joined by `+`.
pub fn languages() -> &'static str {
    static LANGUAGES: OnceLock<String> = OnceLock::new();
    LANGUAGES.get_or_init(|| {
        let wanted = var(OCR_LANGUAGES).unwrap_or_else(|_| "eng+fra+nld+deu".to_string());
        let installed = Command::new("tesseract")
            .arg("--list-langs")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let available = select_languages(&wanted, &installed);
        tracing::info!("OCR languages: {available}");
        available
    })
}

/// The wanted languages present in the output of `tesseract --list-langs` (eng when none is).
fn select_languages(wanted: &str, installed: &str) -> String {
    let installed: Vec<&str> = installed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains(' '))
        .collect();
    let selected: Vec<&str> = wanted
        .split('+')
        .map(str::trim)
        .filter(|l| installed.contains(l))
        .collect();
    if selected.is_empty() {
        "eng".to_string()
    } else {
        selected.join("+")
    }
}

/// Text of an image.
pub fn ocr_image(path: &Path) -> ResultBox<String> {
    let output = Command::new("tesseract")
        .arg(path)
        .arg("stdout")
        .args(["-l", languages()])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "OCR failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Text of a pdf without text layer: its first OCR_MAX_PDF_PAGES pages (default 30) rendered then recognized.
pub fn ocr_pdf(path: &Path) -> ResultBox<String> {
    let max_pages: usize = var(OCR_MAX_PDF_PAGES)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let dir = std::env::temp_dir().join(format!("file-search-ocr-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    let result = (|| -> ResultBox<String> {
        let output = Command::new("pdftoppm")
            .args(["-r", "200", "-gray", "-png", "-l", &max_pages.to_string()])
            .arg(path)
            .arg(dir.join("page"))
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "pdftoppm failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        let mut pages: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "png"))
            .collect();
        // page-01.png, page-02.png...: same width, the name order is the page order
        pages.sort();
        let mut text = String::new();
        for page in pages {
            match ocr_image(&page) {
                Ok(page_text) => {
                    text.push_str(&page_text);
                    text.push('\n');
                }
                Err(e) => tracing::warn!("OCR of {page:?} failed: {e}"),
            }
        }
        Ok(text)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[cfg(test)]
mod tests {
    use super::select_languages;

    #[test]
    fn only_installed_languages() {
        let installed =
            "List of available languages in \"/usr/share/tessdata/\" (3):\neng\nfra\nosd\n";
        assert_eq!(select_languages("eng+fra+nld+deu", installed), "eng+fra");
        assert_eq!(select_languages("nld", installed), "eng");
        assert_eq!(select_languages("fra", ""), "eng");
    }
}
