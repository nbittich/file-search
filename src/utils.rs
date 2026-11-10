use std::path::Path;

pub type Column = usize;
pub type Row = usize;

pub fn convert_row_column_to_letter(mut row: Row, mut column: Column) -> String {
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

/// Lower case extension of a file name ("" when there is none).
pub fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

pub const KIND_PDF: &str = "pdf";
pub const KIND_SPREADSHEET: &str = "spreadsheet";
pub const KIND_CSV: &str = "csv";
pub const KIND_DOCUMENT: &str = "document";
pub const KIND_IMAGE: &str = "image";
pub const KIND_TEXT: &str = "text";

/// Every kind of file indexed.
pub const KINDS: [&str; 6] = [
    KIND_PDF,
    KIND_SPREADSHEET,
    KIND_CSV,
    KIND_DOCUMENT,
    KIND_IMAGE,
    KIND_TEXT,
];

/// The kind of a file from its extension, `None` when the file is not indexed.
pub fn kind_of(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "pdf" => KIND_PDF,
        "xls" | "xlsx" | "xlsm" | "xlsb" | "ods" => KIND_SPREADSHEET,
        "csv" | "tsv" => KIND_CSV,
        "doc" | "docx" | "odt" | "rtf" => KIND_DOCUMENT,
        "png" | "jpg" | "jpeg" | "tif" | "tiff" | "bmp" | "webp" => KIND_IMAGE,
        "txt" | "md" | "json" | "xml" | "log" | "html" | "htm" | "eml" | "ics" | "yaml" | "yml" => {
            KIND_TEXT
        }
        _ => return None,
    })
}

/// Overlapping chunks of words: paragraph sized, so a match keeps its context without huge documents.
pub fn chunks(text: &str) -> Vec<String> {
    const SIZE: usize = 220;
    const STEP: usize = 180;
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut out = Vec::new();
    let mut start = 0;
    while start < words.len() {
        let end = (start + SIZE).min(words.len());
        out.push(words[start..end].join(" "));
        if end == words.len() {
            break;
        }
        start += STEP;
    }
    out
}

/// Enough text to be worth indexing (an image-only pdf gives a few stray characters).
pub fn has_meaningful_text(text: &str) -> bool {
    text.chars().filter(|c| c.is_alphanumeric()).count() >= 32
}

/// The visible text of an html document: tags, scripts and styles removed, entities of the most common characters
/// decoded.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // skip the content of script and style elements
            for skipped in ["script", "style"] {
                if lower[i + 1..].starts_with(skipped) {
                    let close = format!("</{skipped}");
                    if let Some(end) = lower[i..].find(&close) {
                        i += end;
                    }
                }
            }
            match html[i..].find('>') {
                Some(end) => {
                    i += end + 1;
                    out.push(' ');
                }
                None => break,
            }
        } else {
            let next = html[i..].find('<').map(|n| i + n).unwrap_or(html.len());
            out.push_str(&html[i..next]);
            i = next;
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_of_a_cell() {
        assert_eq!(convert_row_column_to_letter(0, 0), "A1");
        assert_eq!(convert_row_column_to_letter(61, 8), "I62");
    }

    #[test]
    fn kinds_from_extensions() {
        assert_eq!(kind_of("xlsm"), Some(KIND_SPREADSHEET));
        assert_eq!(kind_of("tsv"), Some(KIND_CSV));
        assert_eq!(kind_of("jpeg"), Some(KIND_IMAGE));
        assert_eq!(kind_of("exe"), None);
        assert_eq!(extension(Path::new("/a/B.PDF")), "pdf");
        assert_eq!(extension(Path::new("/a/noext")), "");
    }

    #[test]
    fn overlapping_chunks() {
        let text = (0..500)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let chunks = chunks(&text);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].starts_with("0 1 2"));
        assert!(chunks[1].starts_with("180 "));
        assert!(chunks[2].ends_with(" 499"));
        assert!(super::chunks("").is_empty());
        assert_eq!(super::chunks("a b").len(), 1);
    }

    #[test]
    fn text_of_html() {
        let text = html_to_text(
            "<html><head><style>p{color:red}</style><script>var x = 1;</script></head>\
             <body><p>Facture&nbsp;n&deg; <b>42</b> &amp; co</p></body></html>",
        );
        assert!(text.contains("Facture n&deg;"));
        assert!(text.contains("42"));
        assert!(text.contains("& co"));
        assert!(!text.contains("color"));
        assert!(!text.contains("var x"));
    }

    #[test]
    fn meaningful_text() {
        assert!(!has_meaningful_text(" \n\x0c 1 2 "));
        assert!(has_meaningful_text(
            "Invoice 2026-001 for consulting services rendered"
        ));
    }
}
