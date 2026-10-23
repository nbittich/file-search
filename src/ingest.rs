use crate::{index_csv, index_extra, index_pdf, index_tantivy::FileSearchIndex, index_xlsx};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
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
}
#[derive(Default, Clone, Serialize)]
pub struct Stats {
    pub discovered: usize,
    pub indexed: usize,
    pub skipped: usize,
    pub pending: usize,
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
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "pdf"
            | "xls"
            | "xlsx"
            | "xlsb"
            | "ods"
            | "csv"
            | "txt"
            | "md"
            | "json"
            | "xml"
            | "log"
            | "png"
            | "jpg"
            | "jpeg"
            | "tif"
            | "tiff"
            | "docx"
            | "odt"
            | "doc"
    )
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
        Stats {
            discovered: files.len(),
            indexed: files.values().filter(|e| e.status == "indexed").count(),
            skipped: files.values().filter(|e| e.status == "skipped").count(),
            pending: files.values().filter(|e| e.status == "pending").count(),
            files,
        }
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
        while let Some(dir) = dirs.pop() {
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
                let digest = match fingerprint(path) {
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
                    if !dest.exists()
                        && let Err(e) = unpack(path, &dest)
                    {
                        self.record(key, digest, "skipped", Some(e.to_string()))
                            .await?;
                        continue;
                    }
                    self.record(key, digest, "indexed", None).await?;
                    dirs.push(dest);
                    continue;
                }
                if self.entries.lock().await.get(&key).is_some_and(|e| {
                    e.digest == digest && (e.status == "indexed" || e.status == "skipped")
                }) {
                    continue;
                }
                if !supported(path) {
                    self.record(key, digest, "skipped", Some("unsupported format".into()))
                        .await?;
                    continue;
                }
                self.record(key.clone(), digest.clone(), "pending", None)
                    .await?;
                let result = self.process(path.to_path_buf()).await;
                match result {
                    Ok(()) => self.record(key, digest, "indexed", None).await?,
                    Err(e) => {
                        tracing::warn!("skipping {}: {e}", path.display());
                        self.record(key, digest, "skipped", Some(e.to_string()))
                            .await?;
                    }
                }
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
    async fn record(
        &self,
        key: String,
        digest: String,
        status: &str,
        reason: Option<String>,
    ) -> ResultBox<()> {
        self.entries.lock().await.insert(
            key,
            Entry {
                digest,
                status: status.into(),
                reason,
            },
        );
        self.save().await
    }
    pub async fn process(&self, path: PathBuf) -> ResultBox<()> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "pdf" => index_pdf::index_pdf_file(&self.index, path).await,
            "xls" | "xlsx" | "xlsb" | "ods" => index_xlsx::index_xlsx_file(&self.index, path).await,
            "csv" => index_csv::index_csv_file(&self.index, path).await,
            _ => index_extra::index_extra(&self.index, path).await,
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
    fn identifies_compressed_formats() {
        for name in ["a.zip", "a.tar", "a.tar.gz", "a.tgz", "a.gz"] {
            assert!(is_archive(Path::new(name)));
        }
        assert!(!is_archive(Path::new("a.pdf")));
    }
}
