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
