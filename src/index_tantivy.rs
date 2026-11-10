use std::{
    collections::HashSet,
    error::Error,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::Deserialize;
use tantivy::schema::{Document, TantivyDocument, Value};
use tantivy::{
    DateTime, Index, IndexReader, IndexWriter, ReloadPolicy, Term,
    collector::TopDocs,
    query::{AllQuery, Query},
    schema::{
        FAST, Field, INDEXED, IndexRecordOption, NamedFieldDocument, STORED, STRING, Schema,
        TextFieldIndexing, TextOptions,
    },
    tokenizer::{
        AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer,
        TokenStream,
    },
};
use tokio::sync::Mutex;

use crate::utils::{extension, kind_of};

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub enum QueryType {
    TermQuery,
    RegexQuery,
    FuzzySearch,
    QueryParser,
}

/// Version of the schema: a new version is a new index directory (tantivy doesn't migrate a schema), indexed from
/// scratch by the ingestion (its generation marker is new too).
pub const SCHEMA_VERSION: u32 = 2;

/// Tokenizer of the searchable text: words, lower case, accents removed ("Facturé" matches "facture").
pub const FOLDED_TOKENIZER: &str = "folded";

#[derive(Clone)]
pub struct FileSearchIndex {
    pub index: Index,
    pub index_writer: Arc<Mutex<IndexWriter>>,
    pub index_reader: IndexReader,
    pub schema: Schema,
    /// directory of the index (one per schema version)
    pub index_dir: PathBuf,
    pub cell_position_field: Field,
    pub cell_value_field: Field,
    pub cell_ctx_field: Field,
    pub file_name_field: Field,
    pub file_name_text_field: Field,
    pub sheet_name_field: Field,
    pub source_path_field: Field,
    pub origin_path_field: Field,
    pub kind_field: Field,
    pub extension_field: Field,
    pub modified_at_field: Field,
}

/// the text: a chunk of a document, the cells of a row ("label: value")
pub static CELL_VALUE_FIELD: &str = "cell_value";
/// cell positions of a row (A2, B2...) or the chunk ("chunk 3")
pub static CELL_POSITION_FIELD: &str = "cell_position";
/// column labels of a row
pub static CELL_CTX_FIELD: &str = "cell_ctx";
/// file name, exact
pub static FILE_NAME_FIELD: &str = "file_name";
/// file name, searchable words (not stored)
pub static FILE_NAME_TEXT_FIELD: &str = "file_name_text";
pub static SHEET_NAME_FIELD: &str = "sheet_name";
/// the file indexed
pub static SOURCE_PATH_FIELD: &str = "source_path";
/// the uploaded file: the file itself, or the archive it was extracted from
pub static ORIGIN_PATH_FIELD: &str = "origin_path";
/// pdf, spreadsheet, csv, document, image or text
pub static KIND_FIELD: &str = "kind";
/// lower case extension, without dot
pub static EXTENSION_FIELD: &str = "extension";
/// last modification of the uploaded file
pub static MODIFIED_AT_FIELD: &str = "modified_at";

/// What every document of a file shares.
#[derive(Clone, Debug)]
pub struct DocMeta {
    pub source_path: String,
    pub origin_path: String,
    pub file_name: String,
    pub kind: &'static str,
    pub extension: String,
    pub modified_at: Option<DateTime>,
}

impl DocMeta {
    /// The metadata of a file. `origin` is the uploaded archive the file was extracted from, if any: its
    /// modification date is the one of the upload.
    pub fn of(path: &Path, origin: Option<&Path>) -> Result<DocMeta, Box<dyn Error + Send + Sync>> {
        let extension = extension(path);
        let kind = kind_of(&extension).ok_or("unsupported format")?;
        let uploaded = origin.unwrap_or(path);
        let modified_at = std::fs::metadata(uploaded)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| DateTime::from_timestamp_secs(d.as_secs() as i64));
        Ok(DocMeta {
            source_path: path.to_string_lossy().into_owned(),
            origin_path: uploaded.to_string_lossy().into_owned(),
            file_name: path
                .file_name()
                .ok_or("file not found")?
                .to_string_lossy()
                .into_owned(),
            kind,
            extension,
            modified_at,
        })
    }
}

/// Files of an index created before the schema versions (directly in INDEX_DIR_PATH): removed, the new index is in
/// a sub directory and is built again from the uploads.
fn remove_legacy_index(root: &Path) {
    let legacy = |name: &str| {
        name == "meta.json"
            || name == ".managed.json"
            || name == "ingestion-generation"
            || name.starts_with(".tantivy-")
            || [
                ".idx",
                ".pos",
                ".term",
                ".store",
                ".fast",
                ".fieldnorm",
                ".del",
            ]
            .iter()
            .any(|ext| name.ends_with(ext))
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_file() && legacy(&name) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => tracing::info!("removed legacy index file {name}"),
                Err(e) => tracing::warn!("could not remove legacy index file {name}: {e}"),
            }
        }
    }
}

fn folded_text(stored: bool) -> TextOptions {
    let options = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(FOLDED_TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    if stored {
        options.set_stored()
    } else {
        options
    }
}

impl FileSearchIndex {
    /// Opens (or creates) the index in `path/schema-v{SCHEMA_VERSION}`.
    pub fn new(path: &str, writer_memory_arena: usize) -> Result<FileSearchIndex, Box<dyn Error>> {
        let root = PathBuf::from(path);
        std::fs::create_dir_all(&root)?;
        remove_legacy_index(&root);
        let index_dir = root.join(format!("schema-v{SCHEMA_VERSION}"));
        std::fs::create_dir_all(&index_dir)?;

        let mut schema_builder = Schema::builder();
        let cell_position_field =
            schema_builder.add_text_field(CELL_POSITION_FIELD, STRING | STORED);
        let cell_value_field = schema_builder.add_text_field(CELL_VALUE_FIELD, folded_text(true));
        let cell_ctx_field = schema_builder.add_text_field(CELL_CTX_FIELD, folded_text(true));
        let file_name_field = schema_builder.add_text_field(FILE_NAME_FIELD, STRING | STORED);
        let file_name_text_field =
            schema_builder.add_text_field(FILE_NAME_TEXT_FIELD, folded_text(false));
        let sheet_name_field = schema_builder.add_text_field(SHEET_NAME_FIELD, folded_text(true));
        let source_path_field = schema_builder.add_text_field(SOURCE_PATH_FIELD, STRING | STORED);
        let origin_path_field = schema_builder.add_text_field(ORIGIN_PATH_FIELD, STRING | STORED);
        let kind_field = schema_builder.add_text_field(KIND_FIELD, STRING | STORED | FAST);
        let extension_field =
            schema_builder.add_text_field(EXTENSION_FIELD, STRING | STORED | FAST);
        let modified_at_field =
            schema_builder.add_date_field(MODIFIED_AT_FIELD, INDEXED | STORED | FAST);
        let schema = schema_builder.build();
        let index = Index::open_or_create(
            tantivy::directory::MmapDirectory::open(&index_dir)?,
            schema.clone(),
        )?;
        // tokenizers are not stored in the index: registered before any writing or reading
        index.tokenizers().register(
            FOLDED_TOKENIZER,
            TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(RemoveLongFilter::limit(40))
                .filter(LowerCaser)
                .filter(AsciiFoldingFilter)
                .build(),
        );
        let index_writer = Arc::new(Mutex::new(index.writer(writer_memory_arena)?));
        let index_reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(FileSearchIndex {
            schema,
            index,
            index_writer,
            index_reader,
            index_dir,
            cell_value_field,
            cell_ctx_field,
            file_name_field,
            file_name_text_field,
            sheet_name_field,
            cell_position_field,
            source_path_field,
            origin_path_field,
            kind_field,
            extension_field,
            modified_at_field,
        })
    }

    /// A document with the fields shared by every document of a file.
    pub fn new_doc(&self, meta: &DocMeta) -> TantivyDocument {
        let mut doc = TantivyDocument::default();
        doc.add_text(self.file_name_field, &meta.file_name);
        doc.add_text(self.file_name_text_field, &meta.file_name);
        doc.add_text(self.source_path_field, &meta.source_path);
        doc.add_text(self.origin_path_field, &meta.origin_path);
        doc.add_text(self.kind_field, meta.kind);
        doc.add_text(self.extension_field, &meta.extension);
        if let Some(modified_at) = meta.modified_at {
            doc.add_date(self.modified_at_field, modified_at);
        }
        doc
    }

    /// The words of a text, as indexed (lower case, without accents).
    pub fn analyze(&self, text: &str) -> Vec<String> {
        let Some(mut analyzer) = self.index.tokenizers().get(FOLDED_TOKENIZER) else {
            return text.split_whitespace().map(str::to_lowercase).collect();
        };
        let mut tokens = Vec::new();
        let mut stream = analyzer.token_stream(text);
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }

    pub async fn delete_source(&self, source: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut writer = self.index_writer.lock().await;
        writer.delete_term(Term::from_field_text(self.source_path_field, source));
        writer.commit()?;
        Ok(())
    }

    pub async fn reset(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut writer = self.index_writer.lock().await;
        writer.delete_all_documents()?;
        writer.commit()?;
        Ok(())
    }

    pub fn find_all(
        &self,
        page: usize,
        per_page: usize,
    ) -> Result<Vec<NamedFieldDocument>, Box<dyn Error>> {
        self.search_internal(page, per_page, &AllQuery)
    }

    /// The best matching document of each file, `per_page` files from the page.
    pub fn search_internal(
        &self,
        page: usize,
        per_page: usize,
        query: &dyn Query,
    ) -> Result<Vec<NamedFieldDocument>, Box<dyn Error>> {
        Ok(self
            .best_per_file(page, per_page, query)?
            .into_iter()
            .map(|(_, doc)| doc.to_named_doc(&self.schema))
            .collect())
    }

    /// Matches in score order, the best document of each file only, then the page over distinct files.
    pub fn best_per_file(
        &self,
        page: usize,
        per_page: usize,
        query: &dyn Query,
    ) -> Result<Vec<(f32, TantivyDocument)>, Box<dyn Error>> {
        if per_page == 0 {
            return Ok(Vec::new());
        }
        let first = page.checked_mul(per_page).ok_or("page overflow")?;
        let target = first.checked_add(per_page).ok_or("page overflow")?;
        let searcher = self.index_reader.searcher();
        let mut seen = HashSet::new();
        let mut docs = Vec::with_capacity(per_page);
        let mut offset = 0usize;
        let total = searcher.num_docs() as usize;
        while offset < total && seen.len() < target {
            let limit = (total - offset).min(256);
            let hits = searcher.search(
                query,
                &TopDocs::with_limit(limit)
                    .and_offset(offset)
                    .order_by_score(),
            )?;
            if hits.is_empty() {
                break;
            }
            let count = hits.len();
            for (score, address) in hits {
                let document: TantivyDocument = searcher.doc(address)?;
                let source = document
                    .get_first(self.source_path_field)
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!("missing-source:{}:{}", address.segment_ord, address.doc_id)
                    });
                if seen.insert(source) && seen.len() > first {
                    docs.push((score, document));
                    if docs.len() == per_page {
                        return Ok(docs);
                    }
                }
            }
            offset += count;
        }
        Ok(docs)
    }

    /// The legacy search (`/search`): same queries as the v2 search of the mode, every field searched.
    pub fn search(
        &self,
        page: usize,
        per_page: usize,
        q: &str,
        query_type: &QueryType,
    ) -> Result<Vec<NamedFieldDocument>, Box<dyn Error>> {
        let mode = match query_type {
            QueryType::TermQuery => crate::search::SearchMode::Exact,
            QueryType::RegexQuery => crate::search::SearchMode::Regex,
            QueryType::FuzzySearch => crate::search::SearchMode::Fuzzy,
            QueryType::QueryParser => crate::search::SearchMode::Query,
        };
        let request = crate::search::SearchRequest {
            q: q.to_string(),
            mode,
            ..Default::default()
        };
        let query = crate::search::build_query(self, &request).map_err(|e| e.to_string())?;
        self.search_internal(page, per_page, &*query)
    }
}

#[cfg(test)]
pub mod test_support {
    use super::*;

    /// An index in a new temporary directory.
    pub fn temp_index() -> FileSearchIndex {
        let dir = std::env::temp_dir().join(format!("file-search-test-{}", uuid::Uuid::new_v4()));
        FileSearchIndex::new(&dir.to_string_lossy(), 15_000_000).unwrap()
    }

    /// Indexes one document per (file name, kind, text) and waits for the reader to see them.
    pub async fn add_docs(index: &FileSearchIndex, docs: &[(&str, &str, &str)]) {
        let mut writer = index.index_writer.lock().await;
        for (file_name, kind, text) in docs {
            let meta = DocMeta {
                source_path: format!("/uploads/{file_name}"),
                origin_path: format!("/uploads/{file_name}"),
                file_name: file_name.to_string(),
                kind: crate::utils::KINDS
                    .iter()
                    .copied()
                    .find(|k| *k == *kind)
                    .unwrap(),
                extension: extension(Path::new(file_name)),
                modified_at: Some(DateTime::from_timestamp_secs(1_790_000_000)),
            };
            let mut doc = index.new_doc(&meta);
            doc.add_text(index.cell_value_field, *text);
            doc.add_text(index.cell_position_field, "chunk 1");
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();
        index.index_reader.reload().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn words_are_folded() {
        let index = temp_index();
        assert_eq!(
            index.analyze("Facturé à l'ÉTÉ 2026-10"),
            vec!["facture", "a", "l", "ete", "2026", "10"]
        );
    }

    #[test]
    fn the_index_is_in_a_directory_of_its_schema_version() {
        let index = temp_index();
        assert!(
            index
                .index_dir
                .ends_with(format!("schema-v{SCHEMA_VERSION}"))
        );
        assert!(index.index_dir.join("meta.json").exists());
    }

    #[test]
    fn a_legacy_index_is_removed() {
        let dir = std::env::temp_dir().join(format!("file-search-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), "{}").unwrap();
        std::fs::write(dir.join("abc.store"), "x").unwrap();
        std::fs::write(dir.join("ingestion-generation"), "x").unwrap();
        std::fs::write(dir.join("keep.txt"), "x").unwrap();
        let index = FileSearchIndex::new(&dir.to_string_lossy(), 15_000_000).unwrap();
        assert!(!dir.join("meta.json").exists());
        assert!(!dir.join("abc.store").exists());
        assert!(!dir.join("ingestion-generation").exists());
        assert!(dir.join("keep.txt").exists());
        assert!(index.index_dir.join("meta.json").exists());
    }

    #[tokio::test]
    async fn the_legacy_search_finds_every_word() {
        let index = temp_index();
        add_docs(
            &index,
            &[
                ("a.pdf", "pdf", "Facture de consultance octobre"),
                ("b.pdf", "pdf", "Invoice for consulting"),
            ],
        )
        .await;
        // the backoffice used to send the whole query as one fuzzy term: nothing was found with two words
        let docs = index
            .search(0, 10, "facture octobre", &QueryType::FuzzySearch)
            .unwrap();
        assert_eq!(docs.len(), 1);
        let docs = index
            .search(0, 10, "FACTURE", &QueryType::TermQuery)
            .unwrap();
        assert_eq!(docs.len(), 1);
    }
}
