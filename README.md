# File search
Index and search file content (xlsx, csv, pdf)

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
| INDEX_WRITER_SIZE              | 50000000 (50mb)                     |

## install
- use the installation script from the latest [release](https://github.com/nbittich/file-search/releases)
- create file `/etc/systemd/system/file-search.service` and paste the following:


```
[Unit]
Description=File Index Search service
After=network.target
StartLimitIntervalSec=0
[Service]
Environment=RUST_LOG=debug
Environment=INDEX_DIR_PATH=<your-home>/.tantivy_data
Type=simple
Restart=always
RestartSec=1
User=<your-user>
ExecStart=<your-home>/.cargo/bin/file-search

[Install]
WantedBy=multi-user.target
```

- start service: `sudo systemctl start file-search`
- check logs: `sudo journalctl -f -u file-search`

## Automatic ingestion

Mount uploads **read-only** at `UPLOAD_DIR` (default `/uploads`); mount a persistent
writable directory at `INGEST_STATE_DIR` (default `/app/state`). Keep
`INDEX_DIR_PATH` persistent too. The server polls recursively every
`UPLOAD_SCAN_INTERVAL_SECS` (default 10) seconds. Example Docker mounts:

```sh
docker run -p 8080:8080 \
  -v /host/uploads:/uploads:ro \
  -v /host/file-search-state:/app/state \
  -v /host/file-search-index:/index \
  -e INDEX_DIR_PATH=/index file-search
```

A persistent JSON ledger records SHA-256 fingerprints and ingestion status.
Only successful committed ingestions are marked `indexed`; invalid or unsupported
files are recorded as `skipped` with reasons. Replaced files are reindexed;
deleted source files are removed from the index. `GET /stats` reports discovered,
indexed, skipped and pending files, including individual reasons.

`POST /reset` clears the Tantivy index and the ingestion ledger; the background
scanner repopulates it from uploads. If the entire index volume is deleted while
the service is stopped, a generation marker invalidates the ledger at startup.
The legacy `POST /reindex?directory_path=...` resets and rescans the configured
upload directory; its path is only validated for backward compatibility.

ZIP, TAR, TAR.GZ/TGZ and GZ files are unpacked into the *state* volume, never
into read-only uploads. Unsafe archive paths, symlinks and oversized archives
are rejected/ignored. PDFs are indexed in overlapping text chunks; workbooks
include headings, rows, sheets and cell positions. DOC/DOCX/ODT uses
LibreOffice text conversion, while PNG/JPEG/TIFF uses Tesseract OCR.
Images without recognized text are skipped.

**Note:** Existing indexes created before the addition of `source_path` need a
one-time reset/rebuild because Tantivy schemas are not upgraded in place.
