use std::{error::Error, path::PathBuf, sync::Arc};

use serde::Deserialize;
use tantivy::{
    collector::TopDocs,
    query::{FuzzyTermQuery, Query, QueryParser, RegexQuery, TermQuery},
    schema::{Field, IndexRecordOption, Schema, STORED, STRING, TEXT},
    Document, Index, IndexReader, IndexWriter, ReloadPolicy, Term,
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
}

pub static CELL_POSITION_FIELD: &str = "cell_position";
pub static CELL_CTX_FIELD: &str = "cell_ctx";
pub static CELL_VALUE_FIELD: &str = "cell_value";
pub static FILE_NAME_FIELD: &str = "file_name";

impl FileSearchIndex {
    pub fn new(path: &str) -> Result<FileSearchIndex, Box<dyn Error>> {
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
        let schema = schema_builder.build();
        let index = Index::open_or_create(
            tantivy::directory::MmapDirectory::open(&index_dir)?,
            schema.clone(),
        )?;
        let index_writer = Arc::new(Mutex::new(index.writer(50_000_000)?)); // 50mb
        let index_reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommit)
            .try_into()?;
        Ok(FileSearchIndex {
            schema,
            index,
            index_writer,
            index_reader,
            cell_value_field,
            cell_ctx_field,
            file_name_field,
            cell_position_field,
        })
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
                &format!("(?i){q}.*"),
                search_field,
            )?),
            QueryType::FuzzySearch => Box::new(FuzzyTermQuery::new(
                Term::from_field_text(search_field, q),
                2, // todo lehvenstein distance should be a param
                true,
            )),
            QueryType::QueryParser => {
                let query_parser = QueryParser::for_index(&self.index, vec![self.cell_value_field]);
                let query = query_parser.parse_query(q)?;
                Box::new(query)
            }
        };
        Ok(query)
    }
    pub fn search(
        &self,
        page: usize,
        per_page: usize,
        q: &str,
        query_type: &QueryType,
    ) -> Result<Vec<Document>, Box<dyn Error>> {
        let schema = &self.schema;
        let query = self.convert_query_type_to_query(q, query_type)?;
        let searcher = &self.index_reader.searcher();
        let top_docs = searcher.search(&query, &TopDocs::with_limit(per_page).and_offset(page))?;
        let mut docs = Vec::with_capacity(top_docs.len());
        for (_score, doc_address) in top_docs.iter() {
            let retrieved_doc = searcher.doc(*doc_address)?;
            docs.push(retrieved_doc);
        }
        Ok(docs)
    }
}
