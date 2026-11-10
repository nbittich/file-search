# File search

Indexes the content of uploaded files and searches it: pdf (text layer, or OCR of scanned pages), spreadsheets
(xls, xlsx, xlsm, xlsb, ods), csv/tsv, documents (doc, docx, odt, rtf), images (OCR) and text (txt, md, json, xml,
log, html, eml, ics, yaml). Archives (zip, tar, tar.gz/tgz, gz) are unpacked and their files indexed.

Words are matched without case and accents ("facture" finds "Facturé").

## Search

```
GET http://localhost:8080/v2/search?q=facture octobre -brouillon&kinds=pdf&modified_from=2026-01-01
```

| parameter         | default | description                                                                                       |
| ----------------- | ------- | ------------------------------------------------------------------------------------------------- |
| `q`               |         | words, `"exact phrases"`, `-excluded` words. Can be empty when a filter is given                   |
| `mode`            | `smart` | `smart`, `exact`, `prefix`, `fuzzy`, `phrase`, `regex`, `query` (see below)                        |
| `fields`          | all     | comma separated: `content`, `file_name`, `sheet`, `context` (column labels)                         |
| `kinds`           | all     | comma separated: `pdf`, `spreadsheet`, `csv`, `document`, `image`, `text`                          |
| `extensions`      | all     | comma separated: `pdf`, `xlsx`...                                                                  |
| `modified_from`   |         | `yyyy-mm-dd`: files modified this day or after                                                     |
| `modified_to`     |         | `yyyy-mm-dd`: files modified this day or before                                                    |
| `fuzziness`       | auto    | `0`, `1` or `2` letters of difference (`smart`, `fuzzy`). auto: none under 4 letters, 1 under 8   |
| `group_by_file`   | `true`  | `true`: the best match of each file. `false`: every match (each row of a sheet, chunk of a pdf)     |
| `page`/`per_page` | 0 / 20  | zero based page, at most 100 per page                                                              |
| `snippet_chars`   | 240     | length of the highlighted snippet (40 to 1000)                                                     |

Modes:

- `smart`: every word, exactly, as the beginning of a word or with a typo; the words next to each other rank first
- `exact`: every word exactly
- `prefix`: every word exactly or as the beginning of a word
- `fuzzy`: every word exactly or with typos
- `phrase`: the words in this order
- `regex`: a regular expression matching a word (lower case, without accents), e.g. `inv-?202[56]`
- `query`: the [tantivy query syntax](https://docs.rs/tantivy/latest/tantivy/query/struct.QueryParser.html):
  `invoice AND (2026 OR 2027)`, `cell_ctx:amount`, `"frais port"~2`

Response:

```json
{
  "hits": [
    {
      "score": 7.4,
      "source_path": "/app/state/extracted/…/factures/facture-octobre.pdf",
      "origin_path": "/uploads/0c1d2e3f-1234-4abc-8def-0123456789ab.zip",
      "file_name": "facture-octobre.pdf",
      "origin_file_name": "0c1d2e3f-1234-4abc-8def-0123456789ab.zip",
      "kind": "pdf",
      "extension": "pdf",
      "sheet_name": null,
      "positions": ["chunk 1"],
      "context": [],
      "content": "Facture de consultance octobre …",
      "snippet_html": "<b>Facture</b> de consultance <b>octobre</b> …",
      "modified_at": "2026-10-31",
      "matches": 2
    }
  ],
  "page": 0,
  "per_page": 20,
  "has_more": false,
  "total_matches": 2,
  "took_ms": 3
}
```

A wrong parameter (unknown kind, invalid regex or date) answers `400` with `{"error": "..."}`.

`GET /search?page=0&per_page=10&q=sango&query_type=fuzzySearch` (`termQuery`, `regexQuery`, `fuzzySearch`,
`queryParser`) still answers the stored fields of the best match of each file, with the queries of the `exact`,
`regex`, `fuzzy` and `query` modes.

## Other endpoints

- `GET /stats`: discovered, indexed, skipped, duplicate and pending files, indexed files per kind, each file with its
  status and reason
- `GET /find-all?page=0&per_page=10`
- `POST /index` `{"file_path": "/uploads/x.pdf"}`: indexes a file now
- `POST /reset`: clears the index and the ingestion ledger; the scanner indexes every upload again

## Environment variables

| **env**                     | **default value**     |                                                             |
| --------------------------- | --------------------- | ----------------------------------------------------------- |
| SERVICE_HOST                | 0.0.0.0               |                                                             |
| SERVICE_PORT                | 8080                  |                                                             |
| INDEX_DIR_PATH              | `/tmp/__tantivy_data` | the index is in its `schema-v2` sub directory               |
| INDEX_WRITER_SIZE           | 50000000 (50mb)       |                                                             |
| UPLOAD_DIR                  | `/tmp/uploads`        | uploads, read-only                                          |
| INGEST_STATE_DIR            | `/tmp/app/state`      | ledger and unpacked archives                                |
| UPLOAD_SCAN_INTERVAL_SECS   | 10                    |                                                             |
| OCR_LANGUAGES               | `eng+fra+nld+deu`     | the ones installed are used                                 |
| OCR_MAX_PDF_PAGES           | 30                    | pages of a scanned pdf recognized                           |
| RUST_LOG                    | N/A                   |                                                             |

## Run

```sh
docker run -p 8080:8080 \
  -v /host/uploads:/uploads:ro \
  -v /host/file-search-state:/app/state \
  -v /host/file-search-index:/index \
  -e INDEX_DIR_PATH=/index nbittich/file-search
```

The image contains the extraction tools (poppler, tesseract with english/french/dutch/german, LibreOffice).

## Automatic ingestion

Mount uploads **read-only** at `UPLOAD_DIR`; mount a persistent writable directory at `INGEST_STATE_DIR`. Keep
`INDEX_DIR_PATH` persistent too. The server polls recursively every `UPLOAD_SCAN_INTERVAL_SECS` seconds. A file is
hashed again only when its size or modification time changes.

A persistent JSON ledger records SHA-256 fingerprints and ingestion status. Only successful committed ingestions are
marked `indexed`; invalid or unsupported files are recorded as `skipped` with reasons. Replaced files are reindexed;
deleted source files are removed from the index. Files with the same content are indexed once (`duplicate`).

ZIP, TAR, TAR.GZ/TGZ and GZ files are unpacked into the *state* volume, never into read-only uploads. Unsafe archive
paths, symlinks and oversized archives are rejected/ignored. A file of an archive keeps the archive as its
`origin_path`. PDFs and texts are indexed in overlapping chunks; a pdf without text layer is recognized (OCR).
Workbooks are indexed row by row: each cell with the label of its column ("Amount: 1200"), the sheet, the cell
positions; dates as dd/mm/yyyy. The delimiter of a csv (`,` `;` tab `|`) is detected.

## Upgrade

A new schema version is a new index directory (`INDEX_DIR_PATH/schema-vN`): the first start of a new version indexes
every upload again. The files of an index created before the schema versions (directly in `INDEX_DIR_PATH`) are
removed at start.

## Release

`./release.sh <version>`: bumps the version, tags it and pushes. The docker workflow publishes
`nbittich/file-search:latest` (master) and `nbittich/file-search:<version>` (tag), for amd64 and arm64.
