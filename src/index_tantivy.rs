use std::{error::Error, path::PathBuf};

use tantivy::{
    schema::{Field, Schema, STORED, STRING, TEXT},
    Index, IndexReader, IndexWriter, ReloadPolicy,
};

pub struct FileSearchIndex {
    pub index: Index,
    pub index_writer: IndexWriter,
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
        let index_writer = index.writer(50_000_000)?; // 50mb
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
}
