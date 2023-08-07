# File search
Index and search xlsx file content based on tantivy

## endpoints

### Index

```
POST http://localhost:8080/index
{
  "file_path": "/home/nordine/test.xlsx"
}

```

### Search

```
GET http://localhost:8080/search?page=0&per_page=10&q=sango&query_type=regexQuery
```

## environment variables:

| **env**                        | **default value**                   |
| ------------------------------ | ----------------------------------- |
| SERVICE_COLLECTION_NAME        | file-search                         |
| SERVICE_HOST                   | 0.0.0.0                             |
| SERVICE_PORT                   | 8080                                |
| INDEX_DIR_PATH                 | `/tmp/__tantivy_data`               |
| RUST_LOG                       | N/A                                 |
