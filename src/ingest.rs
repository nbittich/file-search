use crate::{
    index_csv, index_extra, index_pdf,
    index_tantivy::{DocMeta, FileSearchIndex},
    index_xlsx,
    utils::{extension, kind_of},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    error::Error,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

type ResultBox<T> = Result<T, Box<dyn Error + Send + Sync>>;
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub digest: String,
    pub status: String,
    pub reason: Option<String>,
    /// size and modification time (ms) when the digest was computed: an unchanged file isn't hashed again
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub modified_ms: u64,
}

/// Size and modification time of a file.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified_ms: u64,
}

fn stamp(metadata: &fs::Metadata) -> Stamp {
    Stamp {
        size: metadata.len(),
        modified_ms: metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default(),
    }
}

/// The ledger is saved every SAVE_EVERY records during a scan (and at its end), not after each file.
const SAVE_EVERY: usize = 50;
#[derive(Default, Clone, Serialize)]
pub struct Stats {
    pub discovered: usize,
    pub indexed: usize,
    pub skipped: usize,
    pub duplicate: usize,
    pub pending: usize,
    /// indexed files per kind (pdf, spreadsheet, csv, document, image, text)
    pub kinds: BTreeMap<String, usize>,
    pub files: BTreeMap<String, Entry>,
}
#[derive(Clone)]
pub struct Ingestor {
    pub index: FileSearchIndex,
    root: PathBuf,
    state: PathBuf,
    pub entries: Arc<Mutex<BTreeMap<String, Entry>>>,
    scan_lock: Arc<Mutex<()>>,
}
fn fingerprint(path: &Path) -> ResultBox<String> {
    let mut f = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}
fn is_archive(path: &Path) -> bool {
    let n = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    n.ends_with(".zip")
        || n.ends_with(".tar")
        || n.ends_with(".tgz")
        || n.ends_with(".tar.gz")
        || n.ends_with(".gz")
}
fn supported(path: &Path) -> bool {
    kind_of(&extension(path)).is_some()
}
fn clean_member(name: &Path) -> ResultBox<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in name.components() {
        match part {
            Component::Normal(s) => out.push(s),
            _ => return Err("unsafe archive path".into()),
        }
    }
    Ok(out)
}
const MAX_FILES: usize = 10000;
const MAX_BYTES: u64 = 1_000_000_000;
fn unpack(archive: &Path, dest: &Path) -> ResultBox<()> {
    let tmp = dest.with_extension("unpacking");
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;
    let operation = (|| -> ResultBox<()> {
        let file = fs::File::open(archive)?;
        let name = archive
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        let mut count = 0usize;
        let mut bytes = 0u64;
        if name.ends_with(".zip") {
            let mut zip = zip::ZipArchive::new(file)?;
            for i in 0..zip.len() {
                let entry = zip.by_index(i)?;
                if entry.is_dir() {
                    continue;
                }
                if entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
                {
                    continue;
                }
                count += 1;
                bytes = bytes.saturating_add(entry.size());
                if count > MAX_FILES || bytes > MAX_BYTES {
                    return Err("archive extraction limit exceeded".into());
                }
                let rel = clean_member(Path::new(&entry.name()?.to_string()))?;
                let target = tmp.join(rel);
                fs::create_dir_all(target.parent().ok_or("invalid path")?)?;
                let mut out = fs::File::create(target)?;
                std::io::copy(&mut entry.take(MAX_BYTES + 1), &mut out)?;
            }
        } else if name.ends_with(".tar") || name.ends_with(".tgz") || name.ends_with(".tar.gz") {
            let input: Box<dyn Read> = if name.ends_with(".tar") {
                Box::new(file)
            } else {
                Box::new(flate2::read::GzDecoder::new(file))
            };
            let mut tar = tar::Archive::new(input);
            for entry in tar.entries()? {
                let entry = entry?;
                if !entry.header().entry_type().is_file() {
                    continue;
                }
                count += 1;
                bytes = bytes.saturating_add(entry.size());
                if count > MAX_FILES || bytes > MAX_BYTES {
                    return Err("archive extraction limit exceeded".into());
                }
                let rel = clean_member(&entry.path()?)?;
                let target = tmp.join(rel);
                fs::create_dir_all(target.parent().ok_or("invalid path")?)?;
                let mut out = fs::File::create(target)?;
                std::io::copy(&mut entry.take(MAX_BYTES + 1), &mut out)?;
            }
        } else if name.ends_with(".gz") {
            let base = archive.file_name().unwrap_or_default().to_string_lossy();
            let output = tmp.join(base.trim_end_matches(".gz"));
            let mut input = flate2::read::GzDecoder::new(file).take(MAX_BYTES + 1);
            let mut out = fs::File::create(output)?;
            if std::io::copy(&mut input, &mut out)? > MAX_BYTES {
                return Err("gzip size exceeded".into());
            }
        } else {
            return Err("not an archive".into());
        }
        Ok(())
    })();
    if let Err(e) = operation {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    fs::rename(tmp, dest)?;
    Ok(())
}
impl Ingestor {
    pub fn new(
        index: FileSearchIndex,
        root: PathBuf,
        state: PathBuf,
        index_dir: &Path,
    ) -> ResultBox<Self> {
        fs::create_dir_all(&state)?;
        let marker = index_dir.join("ingestion-generation");
        let generation = if marker.exists() {
            fs::read_to_string(&marker)?
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            fs::write(&marker, &id)?;
            id
        };
        let ledger_path = state.join("ingestion.json");
        let previous: Option<(String, BTreeMap<String, Entry>)> = fs::read(&ledger_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let entries = match previous {
            Some((g, entries)) if g == generation => entries,
            _ => BTreeMap::new(),
        };
        let state_obj = Self {
            index,
            root,
            state,
            entries: Arc::new(Mutex::new(entries)),
            scan_lock: Arc::new(Mutex::new(())),
        };
        fs::write(state_obj.state.join("generation"), generation)?;
        Ok(state_obj)
    }
    async fn save(&self) -> ResultBox<()> {
        let generation = fs::read_to_string(self.state.join("generation"))?;
        let entries = self.entries.lock().await;
        let contents = serde_json::to_vec(&(generation, &*entries))?;
        let tmp = self.state.join("ingestion.json.tmp");
        fs::write(&tmp, contents)?;
        fs::rename(tmp, self.state.join("ingestion.json"))?;
        Ok(())
    }
    pub async fn stats(&self) -> Stats {
        let files = self.entries.lock().await.clone();
        let mut kinds = BTreeMap::new();
        for (path, _) in files.iter().filter(|(_, e)| e.status == "indexed") {
            // archives are "indexed" too (unpacked): only the files with a kind count
            if let Some(kind) = kind_of(&extension(Path::new(path))) {
                *kinds.entry(kind.to_string()).or_insert(0) += 1;
            }
        }
        Stats {
            discovered: files.len(),
            indexed: files.values().filter(|e| e.status == "indexed").count(),
            skipped: files.values().filter(|e| e.status == "skipped").count(),
            duplicate: files.values().filter(|e| e.status == "duplicate").count(),
            pending: files.values().filter(|e| e.status == "pending").count(),
            kinds,
            files,
        }
    }

    /// The digest of a file: the one of the ledger when its size and modification time didn't change.
    async fn digest(&self, key: &str, path: &Path, stamp: Stamp) -> ResultBox<String> {
        if let Some(previous) = self.entries.lock().await.get(key)
            && !previous.digest.is_empty()
            && previous.size == stamp.size
            && previous.modified_ms == stamp.modified_ms
            && stamp.modified_ms > 0
        {
            return Ok(previous.digest.clone());
        }
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || fingerprint(&path))
            .await
            .map_err(|e| format!("hash worker failed: {e}"))?
    }
    pub async fn reset(&self) -> ResultBox<()> {
        let _scan = self.scan_lock.lock().await;
        self.index.reset().await?;
        self.entries.lock().await.clear();
        self.save().await?;
        Ok(())
    }
    pub async fn scan(&self) -> ResultBox<()> {
        let _scan = self.scan_lock.lock().await;
        if !self.root.exists() {
            return Ok(());
        }
        let mut discovered = HashSet::new();
        let mut dirs = vec![self.root.clone()];
        let mut seen_archives = HashSet::new();
        // directory of an extracted archive -> the uploaded file it comes from
        let mut origins: HashMap<PathBuf, String> = HashMap::new();
        // (path, digest, stamp, uploaded file it comes from when extracted)
        let mut candidates: Vec<(String, String, Stamp, Option<String>)> = Vec::new();
        let mut records = 0usize;
        while let Some(dir) = dirs.pop() {
            let origin = origins.get(&dir).cloned();
            for item in walkdir::WalkDir::new(&dir)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if !item.file_type().is_file() {
                    continue;
                }
                let path = item.path();
                let key = path.to_string_lossy().to_string();
                discovered.insert(key.clone());
                let file_stamp = match item.metadata() {
                    Ok(metadata) => stamp(&metadata),
                    Err(e) => {
                        tracing::warn!("cannot read the metadata of {key}: {e}");
                        continue;
                    }
                };
                let digest = match self.digest(&key, path, file_stamp).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("cannot hash {key}: {e}");
                        continue;
                    }
                };
                if is_archive(path) {
                    let archive_key = format!("{key}:{digest}");
                    if !seen_archives.insert(archive_key.clone()) {
                        continue;
                    }
                    let dest = self
                        .state
                        .join("extracted")
                        .join(hex::encode(Sha256::digest(archive_key.as_bytes())));
                    if !dest.exists() {
                        let (archive, target) = (path.to_path_buf(), dest.clone());
                        let unpacked =
                            tokio::task::spawn_blocking(move || unpack(&archive, &target))
                                .await
                                .map_err(|e| format!("unpack worker failed: {e}"))?;
                        if let Err(e) = unpacked {
                            self.record(key, digest, file_stamp, "skipped", Some(e.to_string()))
                                .await;
                            continue;
                        }
                    }
                    self.record(key.clone(), digest, file_stamp, "indexed", None)
                        .await;
                    // a nested archive comes from the upload of its parent
                    origins.insert(dest.clone(), origin.clone().unwrap_or(key));
                    dirs.push(dest);
                } else {
                    candidates.push((key, digest, file_stamp, origin.clone()));
                }
            }
        }

        // Prefer original upload paths over extracted archive paths. Stable
        // ordering ensures the same content hash keeps the same canonical file.
        let extracted = self.state.join("extracted");
        candidates.sort_by(|a, b| {
            Path::new(&a.0)
                .starts_with(&extracted)
                .cmp(&Path::new(&b.0).starts_with(&extracted))
                .then_with(|| a.0.cmp(&b.0))
        });
        let mut canonical: HashMap<String, String> = HashMap::new();
        for (key, digest, file_stamp, origin) in candidates {
            let previous = self.entries.lock().await.get(&key).cloned();
            if !supported(Path::new(&key)) {
                if previous.as_ref().is_some_and(|e| e.status == "indexed") {
                    self.index.delete_source(&key).await?;
                }
                if !previous
                    .as_ref()
                    .is_some_and(|e| e.digest == digest && e.status == "skipped")
                {
                    self.record(
                        key,
                        digest,
                        file_stamp,
                        "skipped",
                        Some("unsupported format".into()),
                    )
                    .await;
                }
                continue;
            }
            if let Some(source) = canonical.get(&digest) {
                // An older scan could already have indexed this pathname.
                if previous.as_ref().is_some_and(|e| e.status == "indexed") {
                    self.index.delete_source(&key).await?;
                }
                let reason = format!("duplicate content of {source}");
                if !previous.as_ref().is_some_and(|e| {
                    e.digest == digest
                        && e.status == "duplicate"
                        && e.reason.as_deref() == Some(reason.as_str())
                }) {
                    self.record(key, digest, file_stamp, "duplicate", Some(reason))
                        .await;
                }
                continue;
            }
            if previous
                .as_ref()
                .is_some_and(|e| e.digest == digest && e.status == "indexed")
            {
                canonical.insert(digest, key);
                continue;
            }
            if previous
                .as_ref()
                .is_some_and(|e| e.digest == digest && e.status == "skipped")
            {
                continue;
            }
            // Remove stale chunks when the same path has changed content or
            // when an earlier attempt left a partially indexed document.
            if previous.is_some() {
                self.index.delete_source(&key).await?;
            }
            self.record(key.clone(), digest.clone(), file_stamp, "pending", None)
                .await;
            let origin_path = origin.as_deref().map(Path::new);
            match self.process_with_origin(Path::new(&key), origin_path).await {
                Ok(()) => {
                    canonical.insert(digest.clone(), key.clone());
                    self.record(key, digest, file_stamp, "indexed", None).await;
                }
                Err(e) => {
                    tracing::warn!("skipping {key}: {e}");
                    self.index.delete_source(&key).await?;
                    self.record(key, digest, file_stamp, "skipped", Some(e.to_string()))
                        .await;
                }
            }
            records += 1;
            if records % SAVE_EVERY == 0 {
                self.save().await?;
            }
        }
        // Drop deleted source records (including old extracted files).
        let before = self.entries.lock().await.clone();
        for key in before.keys() {
            if !discovered.contains(key) {
                self.index.delete_source(key).await?;
                self.entries.lock().await.remove(key);
            }
        }
        self.save().await?;
        Ok(())
    }

    /// Updates the ledger in memory (saved by the scan).
    async fn record(
        &self,
        key: String,
        digest: String,
        stamp: Stamp,
        status: &str,
        reason: Option<String>,
    ) {
        self.entries.lock().await.insert(
            key,
            Entry {
                digest,
                status: status.into(),
                reason,
                size: stamp.size,
                modified_ms: stamp.modified_ms,
            },
        );
    }

    /// Indexes a file (`POST /index`).
    pub async fn process(&self, path: PathBuf) -> ResultBox<()> {
        self.process_with_origin(&path, None).await
    }

    /// Indexes a file; `origin`: the uploaded archive it was extracted from.
    async fn process_with_origin(&self, path: &Path, origin: Option<&Path>) -> ResultBox<()> {
        let meta = DocMeta::of(path, origin)?;
        match meta.extension.as_str() {
            "pdf" => index_pdf::index_pdf_file(&self.index, path, &meta).await,
            "xls" | "xlsx" | "xlsm" | "xlsb" | "ods" => {
                index_xlsx::index_xlsx_file(&self.index, path, &meta).await
            }
            "csv" | "tsv" => index_csv::index_csv_file(&self.index, path, &meta).await,
            _ => index_extra::index_extra(&self.index, path, &meta).await,
        }
    }
    pub fn start(self) {
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.scan().await {
                    tracing::error!("scan failed: {e}");
                }
                tokio::time::sleep(Duration::from_secs(
                    std::env::var("UPLOAD_SCAN_INTERVAL_SECS")
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(10),
                ))
                .await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unsafe_archive_paths() {
        assert!(clean_member(Path::new("../secret")).is_err());
        assert!(clean_member(Path::new("/absolute")).is_err());
        assert_eq!(
            clean_member(Path::new("safe/report.pdf")).unwrap(),
            PathBuf::from("safe/report.pdf")
        );
    }
    #[test]
    fn identical_file_bytes_share_digest() {
        let dir = std::env::temp_dir().join(format!("file-search-dedup-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let a = dir.join("original.txt");
        let b = dir.join("extracted.txt");
        fs::write(&a, b"same content").unwrap();
        fs::write(&b, b"same content").unwrap();
        assert_eq!(fingerprint(&a).unwrap(), fingerprint(&b).unwrap());
        fs::write(&b, b"changed content").unwrap();
        assert_ne!(fingerprint(&a).unwrap(), fingerprint(&b).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn uploads_are_indexed_with_their_origin() {
        use crate::search::{SearchMode, SearchRequest, search};
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("file-search-ingest-{}", uuid::Uuid::new_v4()));
        let uploads = dir.join("uploads");
        fs::create_dir_all(&uploads).unwrap();
        fs::write(uploads.join("notes.txt"), "Réunion avec le comptable").unwrap();
        fs::write(uploads.join("export.csv"), "Client;Montant\nACME;1200\n").unwrap();
        let zip_path = uploads.join("0c1d2e3f-1234-4abc-8def-0123456789ab.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        zip.start_file(
            "factures/facture-octobre.txt",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"Facture de consultance octobre").unwrap();
        zip.finish().unwrap();
        fs::write(uploads.join("program.exe"), "binary").unwrap();

        let index = crate::index_tantivy::test_support::temp_index();
        let index_dir = index.index_dir.clone();
        let ingestor =
            Ingestor::new(index, uploads.clone(), dir.join("state"), &index_dir).unwrap();
        ingestor.scan().await.unwrap();
        ingestor.index.index_reader.reload().unwrap();

        let stats = ingestor.stats().await;
        assert_eq!(stats.kinds.get("text"), Some(&2));
        assert_eq!(stats.kinds.get("csv"), Some(&1));
        assert_eq!(stats.skipped, 1, "the exe");

        let find = |q: &str| {
            search(
                &ingestor.index,
                &SearchRequest {
                    q: q.into(),
                    mode: SearchMode::Smart,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        assert_eq!(find("reunion").hits[0].file_name, "notes.txt");
        let csv = find("acme");
        assert_eq!(csv.hits[0].content, "Client: ACME | Montant: 1200");
        assert_eq!(csv.hits[0].context, vec!["Client", "Montant"]);
        let extracted = &find("consultance").hits[0];
        assert_eq!(extracted.file_name, "facture-octobre.txt");
        assert_eq!(
            extracted.origin_file_name,
            "0c1d2e3f-1234-4abc-8def-0123456789ab.zip"
        );

        // an unchanged upload is neither hashed nor indexed again
        let digests: Vec<(String, u64)> = ingestor
            .entries
            .lock()
            .await
            .iter()
            .map(|(k, e)| (k.clone(), e.size))
            .collect();
        ingestor.scan().await.unwrap();
        let after: Vec<(String, u64)> = ingestor
            .entries
            .lock()
            .await
            .iter()
            .map(|(k, e)| (k.clone(), e.size))
            .collect();
        assert_eq!(digests, after);
        assert!(digests.iter().all(|(_, size)| *size > 0));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn identifies_compressed_formats() {
        for name in ["a.zip", "a.tar", "a.tar.gz", "a.tgz", "a.gz"] {
            assert!(is_archive(Path::new(name)));
        }
        assert!(!is_archive(Path::new("a.pdf")));
    }
}
