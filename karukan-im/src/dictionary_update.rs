//! Safe background updates for the published Karukan system dictionary.
//!
//! The updater downloads a small HTTPS manifest first, then streams the
//! referenced `dict.tgz` into the data directory. Size, SHA-256, archive
//! structure, and the KRKN dictionary format are all verified before the
//! installed `dict.bin` is atomically replaced. User dictionaries and the
//! learning cache live at different paths and are never touched here.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use fs2::FileExt;
use karukan_engine::Dictionary;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Settings;

const MANIFEST_SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DICTIONARY_BYTES: u64 = 512 * 1024 * 1024;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Published metadata for one compressed Karukan system dictionary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DictionaryManifest {
    pub schema_version: u32,
    /// Karukan dictionary build identifier.
    pub version: String,
    /// Upstream SudachiDict release tag, when known.
    pub source_version: Option<String>,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub archive: DictionaryArchive,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DictionaryArchive {
    TarGzip,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct DictionaryUpdateState {
    last_checked_unix: u64,
    installed_version: Option<String>,
    archive_sha256: Option<String>,
}

/// Result of one automatic or manual update attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictionaryUpdateOutcome {
    Updated { version: String, path: PathBuf },
    UpToDate { version: String },
    Skipped { reason: String },
}

/// A completed background check. Updated dictionaries are loaded on the
/// worker thread so the input path only has to swap the ready-to-use value.
pub struct BackgroundDictionaryUpdate {
    pub outcome: DictionaryUpdateOutcome,
    pub dictionary: Option<Dictionary>,
}

impl std::fmt::Display for DictionaryUpdateOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Updated { version, path } => {
                write!(formatter, "updated to {version}: {}", path.display())
            }
            Self::UpToDate { version } => write!(formatter, "already up to date: {version}"),
            Self::Skipped { reason } => write!(formatter, "skipped: {reason}"),
        }
    }
}

/// Start one detached update check and return a receiver for its result.
/// The engine polls this receiver before processing keys, so network and
/// archive work never block typing.
pub fn spawn_background_update(
    settings: &Settings,
) -> Option<Receiver<Result<BackgroundDictionaryUpdate, String>>> {
    if !settings.dictionary_update.enabled
        || settings.conversion.dict_path.is_some()
        || std::env::var_os("KARUKAN_DISABLE_DICTIONARY_UPDATE").is_some()
    {
        return None;
    }

    let settings = settings.clone();
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("karukan-dict-update".to_string())
        .spawn(move || {
            loop {
                let result = update_dictionary(&settings, false)
                    .and_then(|outcome| {
                        let dictionary = match &outcome {
                            DictionaryUpdateOutcome::Updated { path, .. } => {
                                Some(Dictionary::load(path).with_context(|| {
                                    format!("failed to load {}", path.display())
                                })?)
                            }
                            _ => None,
                        };
                        Ok(BackgroundDictionaryUpdate {
                            outcome,
                            dictionary,
                        })
                    })
                    .map_err(|error| format!("{error:#}"));
                if sender.send(result).is_err() {
                    break;
                }
                std::thread::sleep(background_poll_interval(&settings));
            }
        })
        .ok()?;
    Some(receiver)
}

/// Check and install the newest published system dictionary.
///
/// `force` bypasses the interval and disabled setting, but never overwrites a
/// custom `conversion.dict_path`; custom dictionaries remain user-owned.
pub fn update_dictionary(settings: &Settings, force: bool) -> Result<DictionaryUpdateOutcome> {
    if std::env::var_os("KARUKAN_DISABLE_DICTIONARY_UPDATE").is_some() {
        return Ok(DictionaryUpdateOutcome::Skipped {
            reason: "disabled by KARUKAN_DISABLE_DICTIONARY_UPDATE".to_string(),
        });
    }
    if !settings.dictionary_update.enabled && !force {
        return Ok(DictionaryUpdateOutcome::Skipped {
            reason: "disabled in config.toml".to_string(),
        });
    }
    if settings.conversion.dict_path.is_some() {
        return Ok(DictionaryUpdateOutcome::Skipped {
            reason: "conversion.dict_path is custom; refusing to overwrite it".to_string(),
        });
    }

    let data_dir =
        Settings::data_dir().context("could not determine the Karukan data directory")?;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create {}", data_dir.display()))?;
    let lock_path = data_dir.join("dictionary-update.lock");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    if let Err(error) = lock_file.try_lock_exclusive() {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(DictionaryUpdateOutcome::Skipped {
                reason: "another Karukan process is updating the dictionary".to_string(),
            });
        }
        return Err(error).context("failed to lock the system dictionary updater");
    }

    let destination = data_dir.join("dict.bin");
    let state_path = data_dir.join("dictionary-update-state.json");
    let now = now_unix();
    let mut state = read_state(&state_path).unwrap_or_default();
    let interval_seconds = settings
        .dictionary_update
        .check_interval_hours
        .saturating_mul(60 * 60);
    if !force && !check_is_due(&state, now, interval_seconds) {
        return Ok(DictionaryUpdateOutcome::Skipped {
            reason: "checked recently".to_string(),
        });
    }

    validate_https_url(
        &settings.dictionary_update.manifest_url,
        "dictionary manifest",
    )?;
    let timeout = Duration::from_secs(settings.dictionary_update.timeout_seconds.max(1));
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .build();
    let manifest = fetch_manifest(&agent, &settings.dictionary_update.manifest_url)?;
    validate_manifest(&manifest)?;

    if destination.is_file()
        && state.installed_version.as_deref() == Some(&manifest.version)
        && state.archive_sha256.as_deref() == Some(&manifest.sha256)
        && Dictionary::load(&destination).is_ok()
    {
        state.last_checked_unix = now;
        write_state(&state_path, &state)?;
        return Ok(DictionaryUpdateOutcome::UpToDate {
            version: manifest.version,
        });
    }

    let archive_path = unique_temp_path(&destination, "download");
    let update_result = (|| -> Result<DictionaryUpdateOutcome> {
        download_archive(&agent, &manifest, &archive_path)?;
        install_verified_archive(&archive_path, &manifest, &destination)?;
        state.last_checked_unix = now;
        state.installed_version = Some(manifest.version.clone());
        state.archive_sha256 = Some(manifest.sha256.clone());
        if let Err(error) = write_state(&state_path, &state) {
            tracing::warn!(
                "Dictionary was installed, but update state could not be saved at {}: {error:#}",
                state_path.display()
            );
        }
        Ok(DictionaryUpdateOutcome::Updated {
            version: manifest.version,
            path: destination,
        })
    })();
    let _ = std::fs::remove_file(&archive_path);
    update_result
}

fn fetch_manifest(agent: &ureq::Agent, url: &str) -> Result<DictionaryManifest> {
    let response = agent
        .get(url)
        .set("User-Agent", "karukan-im/dictionary-updater")
        .call()
        .with_context(|| format!("failed to fetch dictionary manifest from {url}"))?;
    validate_https_url(response.get_url(), "final dictionary manifest")?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("failed to read dictionary manifest")?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("dictionary manifest is too large");
    }
    serde_json::from_slice(&bytes).context("invalid dictionary manifest JSON")
}

fn validate_manifest(manifest: &DictionaryManifest) -> Result<()> {
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        bail!(
            "unsupported dictionary manifest schema {}",
            manifest.schema_version
        );
    }
    if manifest.version.trim().is_empty() || manifest.version.len() > 128 {
        bail!("invalid dictionary version");
    }
    validate_https_url(&manifest.url, "dictionary archive")?;
    if manifest.size == 0 || manifest.size > MAX_ARCHIVE_BYTES {
        bail!("invalid dictionary archive size: {}", manifest.size);
    }
    if manifest.sha256.len() != 64 || !manifest.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("invalid dictionary archive SHA-256");
    }
    Ok(())
}

fn validate_https_url(url: &str, label: &str) -> Result<()> {
    if !url.starts_with("https://") || url.chars().any(char::is_whitespace) {
        bail!("{label} URL must use HTTPS");
    }
    Ok(())
}

fn download_archive(agent: &ureq::Agent, manifest: &DictionaryManifest, path: &Path) -> Result<()> {
    let response = agent
        .get(&manifest.url)
        .set("User-Agent", "karukan-im/dictionary-updater")
        .call()
        .with_context(|| format!("failed to download dictionary from {}", manifest.url))?;
    validate_https_url(response.get_url(), "final dictionary archive")?;
    let mut reader = response.into_reader();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .context("dictionary download failed")?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > manifest.size || total > MAX_ARCHIVE_BYTES {
            bail!("dictionary archive exceeded its declared size");
        }
        hasher.update(&buffer[..count]);
        file.write_all(&buffer[..count])?;
    }
    file.flush()?;
    file.sync_all()?;

    if total != manifest.size {
        bail!(
            "dictionary archive size mismatch: expected {}, got {total}",
            manifest.size
        );
    }
    let actual_sha256 = format!("{:x}", hasher.finalize());
    if !actual_sha256.eq_ignore_ascii_case(&manifest.sha256) {
        bail!("dictionary archive SHA-256 mismatch");
    }
    Ok(())
}

fn install_verified_archive(
    archive_path: &Path,
    manifest: &DictionaryManifest,
    destination: &Path,
) -> Result<()> {
    let actual_size = std::fs::metadata(archive_path)?.len();
    if actual_size != manifest.size {
        bail!("downloaded archive changed before installation");
    }
    let actual_sha256 = sha256_file(archive_path)?;
    if !actual_sha256.eq_ignore_ascii_case(&manifest.sha256) {
        bail!("downloaded archive failed SHA-256 revalidation");
    }

    let extracted_path = unique_temp_path(destination, "extracted");
    let extraction_result = extract_dictionary(archive_path, manifest.archive, &extracted_path);
    if let Err(error) = extraction_result {
        let _ = std::fs::remove_file(&extracted_path);
        return Err(error);
    }

    let install_result = (|| -> Result<()> {
        // Fully parse the downloaded dictionary before it can replace the
        // working one. This enforces all KRKN size and format bounds.
        Dictionary::load(&extracted_path).context("downloaded dict.bin is invalid")?;

        if destination.is_file() {
            let backup_path = destination.with_extension("bin.previous");
            let backup_temp = unique_temp_path(&backup_path, "backup");
            let backup_result = (|| -> Result<()> {
                std::fs::copy(destination, &backup_temp)
                    .with_context(|| format!("failed to back up {}", destination.display()))?;
                File::open(&backup_temp)?.sync_all()?;
                std::fs::rename(&backup_temp, &backup_path).with_context(|| {
                    format!("failed to publish backup {}", backup_path.display())
                })?;
                Ok(())
            })();
            if backup_result.is_err() {
                let _ = std::fs::remove_file(&backup_temp);
            }
            backup_result?;
        }

        std::fs::rename(&extracted_path, destination).with_context(|| {
            format!("failed to replace dictionary at {}", destination.display())
        })?;
        sync_parent(destination);
        Ok(())
    })();
    if install_result.is_err() {
        let _ = std::fs::remove_file(&extracted_path);
    }
    install_result
}

fn extract_dictionary(
    archive_path: &Path,
    archive_format: DictionaryArchive,
    output_path: &Path,
) -> Result<()> {
    let archive_file = File::open(archive_path)?;
    let reader: Box<dyn Read> = match archive_format {
        DictionaryArchive::TarGzip => Box::new(GzDecoder::new(archive_file)),
    };
    let mut archive = tar::Archive::new(reader);
    let mut found = false;
    for entry in archive
        .entries()
        .context("failed to read dictionary archive")?
    {
        let entry = entry.context("failed to read dictionary archive entry")?;
        let path = entry.path().context("invalid dictionary archive path")?;
        let is_dictionary = path == Path::new("dict.bin") || path == Path::new("./dict.bin");
        if !is_dictionary {
            continue;
        }
        if found || !entry.header().entry_type().is_file() {
            bail!("dictionary archive contains an invalid dict.bin entry");
        }
        found = true;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output_path)?;
        let copied = std::io::copy(&mut entry.take(MAX_DICTIONARY_BYTES + 1), &mut output)?;
        if copied > MAX_DICTIONARY_BYTES {
            bail!("extracted dictionary is too large");
        }
        output.flush()?;
        output.sync_all()?;
    }
    if !found {
        bail!("dictionary archive does not contain dict.bin");
    }
    Ok(())
}

fn check_is_due(state: &DictionaryUpdateState, now: u64, interval_seconds: u64) -> bool {
    state.last_checked_unix == 0
        || now < state.last_checked_unix
        || now.saturating_sub(state.last_checked_unix) >= interval_seconds
}

fn background_poll_interval(settings: &Settings) -> Duration {
    let configured = settings
        .dictionary_update
        .check_interval_hours
        .saturating_mul(60 * 60);
    // A zero interval means "check at every process start". Avoid a tight
    // loop after the initial check while still refreshing long-lived IMEs.
    // Otherwise, polling the local state hourly is enough; update_dictionary
    // performs the actual network interval gating.
    let poll_seconds = if configured == 0 {
        24 * 60 * 60
    } else {
        configured.min(60 * 60)
    };
    Duration::from_secs(poll_seconds)
}

fn read_state(path: &Path) -> Result<DictionaryUpdateState> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).context("invalid dictionary update state")
}

fn write_state(path: &Path, state: &DictionaryUpdateState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary_path = unique_temp_path(path, "state");
    let result = (|| -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(&temporary_path, path)?;
        sync_parent(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

fn unique_temp_path(target: &Path, purpose: &str) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("dict.bin");
    let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{name}.{}.{}.{}.tmp",
        std::process::id(),
        sequence,
        purpose
    ))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(directory) = File::open(parent)
    {
        let _ = directory.sync_all();
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tempfile::tempdir;

    fn create_dictionary_archive(directory: &Path) -> (PathBuf, DictionaryManifest) {
        let json_path = directory.join("source.json");
        std::fs::write(
            &json_path,
            r#"[{"reading":"あい","candidates":[{"surface":"愛","score":1.0}]}]"#,
        )
        .unwrap();
        let dictionary_path = directory.join("built-dict.bin");
        Dictionary::build_from_json(&json_path)
            .unwrap()
            .save(&dictionary_path)
            .unwrap();

        let archive_path = directory.join("dict.tgz");
        let archive_file = File::create(&archive_path).unwrap();
        let encoder = GzEncoder::new(archive_file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        builder
            .append_path_with_name(&dictionary_path, "dict.bin")
            .unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();

        let manifest = DictionaryManifest {
            schema_version: 1,
            version: "test-1".to_string(),
            source_version: Some("test-source".to_string()),
            url: "https://example.com/dict.tgz".to_string(),
            sha256: sha256_file(&archive_path).unwrap(),
            size: std::fs::metadata(&archive_path).unwrap().len(),
            archive: DictionaryArchive::TarGzip,
        };
        (archive_path, manifest)
    }

    #[test]
    fn manifest_requires_https_and_valid_digest() {
        let manifest = DictionaryManifest {
            schema_version: 1,
            version: "test".to_string(),
            source_version: None,
            url: "http://example.com/dict.tgz".to_string(),
            sha256: "x".repeat(64),
            size: 1,
            archive: DictionaryArchive::TarGzip,
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn update_interval_handles_clock_rollback() {
        let state = DictionaryUpdateState {
            last_checked_unix: 1_000,
            ..DictionaryUpdateState::default()
        };
        assert!(!check_is_due(&state, 1_100, 200));
        assert!(check_is_due(&state, 1_200, 200));
        assert!(check_is_due(&state, 900, 200));
    }

    #[test]
    fn background_polling_is_bounded() {
        let mut settings = Settings::default();
        settings.dictionary_update.check_interval_hours = 24;
        assert_eq!(
            background_poll_interval(&settings),
            Duration::from_secs(3600)
        );
        settings.dictionary_update.check_interval_hours = 0;
        assert_eq!(
            background_poll_interval(&settings),
            Duration::from_secs(86_400)
        );
    }

    #[test]
    fn custom_dictionary_is_never_updated() {
        let mut settings = Settings::default();
        settings.conversion.dict_path = Some("/owned/custom-dict.bin".to_string());
        let outcome = update_dictionary(&settings, true).unwrap();
        assert!(matches!(outcome, DictionaryUpdateOutcome::Skipped { .. }));
    }

    #[test]
    fn verified_archive_replaces_dictionary_and_keeps_backup() {
        let directory = tempdir().unwrap();
        let (archive_path, manifest) = create_dictionary_archive(directory.path());
        let destination = directory.path().join("dict.bin");
        std::fs::write(&destination, b"old dictionary").unwrap();

        install_verified_archive(&archive_path, &manifest, &destination).unwrap();

        let dictionary = Dictionary::load(&destination).unwrap();
        let result = dictionary.exact_match_search("あい").unwrap();
        assert_eq!(result.candidates[0].surface, "愛");
        assert_eq!(
            std::fs::read(destination.with_extension("bin.previous")).unwrap(),
            b"old dictionary"
        );
    }

    #[test]
    fn archive_digest_is_revalidated_before_installation() {
        let directory = tempdir().unwrap();
        let (archive_path, mut manifest) = create_dictionary_archive(directory.path());
        manifest.sha256 = "0".repeat(64);
        let destination = directory.path().join("dict.bin");

        assert!(install_verified_archive(&archive_path, &manifest, &destination).is_err());
        assert!(!destination.exists());
    }
}
