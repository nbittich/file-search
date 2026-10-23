use std::{collections::HashSet, error::Error, path::PathBuf, sync::Arc};

use serde::Deserialize;
use tantivy::schema::{Document, TantivyDocument, Value};
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, Term,
    collector::TopDocs,
    query::{AllQuery, FuzzyTermQuery, Query, QueryParser, RegexQuery, TermQuery},
    schema::{Field, IndexRecordOption, NamedFieldDocument, STORED, STRING, Schema, TEXT},
};
use tokio::sync::Mutex;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QueryType {
    TermQuery,
    RegexQuery,
    FuzzySearch,
    QueryParser,
}

#[derive(Clone)]
pub struct FileSearchIndex {
    pub index: Index,
    pub index_writer: Arc<Mutex<IndexWriter>>,
    pub index_reader: IndexReader,
    pub schema: Schema,
    pub cell_position_field: Field,
    pub cell_value_field: Field,
    pub cell_ctx_field: Field,
    pub file_name_field: Field,
    pub sheet_name_field: Field,
    pub source_path_field: Field,
}

pub static CELL_POSITION_FIELD: &str = "cell_position";
pub static CELL_CTX_FIELD: &str = "cell_ctx";
pub static CELL_VALUE_FIELD: &str = "cell_value";
pub static FILE_NAME_FIELD: &str = "file_name";
pub static SHEET_NAME_FIELD: &str = "sheet_name";
pub static SOURCE_PATH_FIELD: &str = "source_path";

impl FileSearchIndex {
    pub fn new(path: &str, writer_memory_arena: usize) -> Result<FileSearchIndex, Box<dyn Error>> {
        let index_dir = PathBuf::from(path);
        if !index_dir.exists() {
            std::fs::create_dir(index_dir.as_path())?;
        }
        let mut schema_builder = Schema::builder();
        let cell_position_field =
            schema_builder.add_text_field(CELL_POSITION_FIELD, STRING | STORED);
        let cell_value_field = schema_builder.add_text_field(CELL_VALUE_FIELD, TEXT | STORED);
        let cell_ctx_field = schema_builder.add_text_field(CELL_CTX_FIELD, STRING | STORED);
        let file_name_field = schema_builder.add_text_field(FILE_NAME_FIELD, STRING | STORED);
        let sheet_name_field = schema_builder.add_text_field(SHEET_NAME_FIELD, TEXT | STORED);
        let source_path_field = schema_builder.add_text_field(SOURCE_PATH_FIELD, STRING | STORED);
        let schema = schema_builder.build();
        let index = Index::open_or_create(
            tantivy::directory::MmapDirectory::open(&index_dir)?,
            schema.clone(),
        )?;
        let index_writer = Arc::new(Mutex::new(index.writer(writer_memory_arena)?)); // 50mb
        let index_reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(FileSearchIndex {
            schema,
            index,
            index_writer,
            index_reader,
            cell_value_field,
            cell_ctx_field,
            file_name_field,
            sheet_name_field,
            cell_position_field,
            source_path_field,
        })
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

    pub fn convert_query_type_to_query(
        &self,
        q: &str,
        query_type: &QueryType,
    ) -> Result<Box<dyn Query>, Box<dyn Error>> {
        let search_field = self.cell_value_field;
        let query: Box<dyn Query> = match query_type {
            QueryType::TermQuery => Box::new(TermQuery::new(
                Term::from_field_text(search_field, q),
                IndexRecordOption::Basic,
            )),
            QueryType::RegexQuery => Box::new(RegexQuery::from_pattern(
                // &format!("(?i){q}.*"), // this should be done by the client
                q,
                search_field,
            )?),
            QueryType::FuzzySearch => Box::new(FuzzyTermQuery::new(
                Term::from_field_text(search_field, q),
                2, // todo lehvenstein distance should be a param
                true,
            )),
            QueryType::QueryParser => {
                let query_parser = QueryParser::for_index(
                    &self.index,
                    vec![
                        self.cell_value_field,
                        self.cell_ctx_field,
                        self.sheet_name_field,
                    ],
                );
                let query = query_parser.parse_query(q)?;
                Box::new(query)
            }
        };
        Ok(query)
    }
    pub fn find_all(
        &self,
        page: usize,
        per_page: usize,
    ) -> Result<Vec<NamedFieldDocument>, Box<dyn Error>> {
        let query = AllQuery;
        self.search_internal(page, per_page, query)
    }

    fn search_internal(
        &self,
        page: usize,
        per_page: usize,
        query: impl Query,
    ) -> Result<Vec<NamedFieldDocument>, Box<dyn Error>> {
        if per_page == 0 {
            return Ok(Vec::new());
        }
        let first = page.checked_mul(per_page).ok_or("page overflow")?;
        let target = first.checked_add(per_page).ok_or("page overflow")?;
        let schema = &self.schema;
        let searcher = self.index_reader.searcher();
        let mut seen = HashSet::new();
        let mut docs = Vec::with_capacity(per_page);
        let mut offset = 0usize;
        let total = searcher.num_docs() as usize;
        // Process matches in score order; keep the best matching chunk for
        // each source, then paginate over DISTINCT source files.
        while offset < total && seen.len() < target {
            let limit = (total - offset).min(256);
            let hits = searcher.search(
                &query,
                &TopDocs::with_limit(limit)
                    .and_offset(offset)
                    .order_by_score(),
            )?;
            if hits.is_empty() {
                break;
            }
            let count = hits.len();
            for (_score, address) in hits {
                let document: TantivyDocument = searcher.doc(address)?;
                let source = document
                    .get_first(self.source_path_field)
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!("missing-source:{}:{}", address.segment_ord, address.doc_id)
                    });
                if seen.insert(source) && seen.len() > first {
                    docs.push(document.to_named_doc(schema));
                    if docs.len() == per_page {
                        return Ok(docs);
                    }
                }
            }
            offset += count;
        }
        Ok(docs)
    }
    pub fn search(
        &self,
        page: usize,
        per_page: usize,
        q: &str,
        query_type: &QueryType,
    ) -> Result<Vec<NamedFieldDocument>, Box<dyn Error>> {
        let query = self.convert_query_type_to_query(q, query_type)?;

        self.search_internal(page, per_page, query)
    }
}
