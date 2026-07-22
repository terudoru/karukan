//! Engine initialization (model loading, dictionary setup)

use anyhow::{Context, Result};
use tracing::debug;

use crate::config::settings::StrategyMode;

use super::*;

/// Create a KanaKanjiConverter from a variant id, optionally setting thread count.
fn create_converter(variant_id: &str, n_threads: u32) -> Result<KanaKanjiConverter> {
    let backend = karukan_engine::Backend::from_variant_id(variant_id)?;
    let mut converter = KanaKanjiConverter::new(backend)?;
    if n_threads > 0 {
        converter.set_n_threads(n_threads);
    }
    Ok(converter)
}

/// Format the n_threads value for debug logging.
fn threads_label(n_threads: u32) -> String {
    if n_threads > 0 {
        n_threads.to_string()
    } else {
        "default".to_string()
    }
}

impl InputMethodEngine {
    /// Full engine initialization from user settings: system dictionary,
    /// user dictionaries, learning cache, and conversion models according
    /// to the configured strategy.
    ///
    /// Used by the fcitx5 FFI (`karukan_engine_init`) and other synchronous
    /// callers. The macOS stdio server uses [`Self::init_from_settings_async`]
    /// so a cold model load cannot block its first key. In `Adaptive` mode a
    /// light-model failure is non-fatal (beam search is simply unavailable).
    pub fn init_from_settings(&mut self, settings: &Settings) -> Result<()> {
        self.init_non_model_state(settings);
        let loaded = load_model_converters(settings)?;
        self.install_model_converters(loaded);
        tracing::info!("Karukan init complete: {}", self.model_name());
        Ok(())
    }

    /// Build dictionaries, learning data, and conversion models on a worker
    /// thread. This is the macOS stdio path: romaji/kana input can respond
    /// immediately even when a cold dictionary or Metal model load takes
    /// seconds. The completed resources are installed by
    /// [`Self::poll_resource_initialization`] between key events.
    pub fn init_from_settings_async(&mut self, settings: &Settings) -> Result<()> {
        if self.converters.kanji.is_some() || self.resource_initialization.is_some() {
            return Ok(());
        }

        let settings = settings.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("karukan-resource-init".to_string())
            .spawn(move || {
                let config = EngineConfig::from_settings(&settings);
                let mut loader = InputMethodEngine::with_config(config);
                let result = loader
                    .init_from_settings(&settings)
                    .map(|()| Box::new(loader))
                    .map_err(|error| format!("{error:#}"));
                let _ = sender.send(result);
            })
            .context("failed to start resource initialization thread")?;
        self.resource_initialization = Some(receiver);
        Ok(())
    }

    fn init_non_model_state(&mut self, settings: &Settings) {
        tracing::info!(
            "Karukan init: model={:?}, light_model={:?}, strategy={:?}",
            settings.conversion.model,
            settings.conversion.light_model,
            settings.conversion.strategy,
        );

        self.init_system_dictionary(settings.conversion.dict_path.as_deref());
        self.dictionary_update = spawn_background_update(settings);
        self.init_user_dictionaries();
        self.init_learning_cache(
            settings.learning.enabled,
            LearningConfig {
                max_entries: settings.learning.max_entries,
                max_surface_chars: settings.learning.max_surface_chars,
            },
        );
    }

    fn install_model_converters(&mut self, loaded: Converters) {
        self.converters.kanji = loaded.kanji;
        self.converters.light_kanji = loaded.light_kanji;
        // A no-model refresh may have cached pass-through chunks while the
        // worker was loading. Force the next idle refresh to use the models.
        self.chunks.clear();
    }

    fn install_initialized_resources(&mut self, mut loaded: InputMethodEngine) {
        self.converters.kanji = loaded.converters.kanji.take();
        self.converters.light_kanji = loaded.converters.light_kanji.take();
        self.dicts = loaded.dicts;
        self.learning = loaded.learning;
        self.dictionary_update = loaded.dictionary_update.take();
        // A pre-init refresh may have cached pass-through chunks. Force the
        // next idle refresh to use the newly installed dictionaries/models.
        self.chunks.clear();
    }

    /// Install completed asynchronous resources without ever waiting in a key
    /// callback.
    pub(crate) fn poll_resource_initialization(&mut self) {
        let Some(receiver) = self.resource_initialization.as_ref() else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.resource_initialization = None;
                tracing::warn!("Resource initialization worker disconnected");
                return;
            }
        };
        let Some(result) = result else { return };
        self.resource_initialization = None;
        match result {
            Ok(loaded) => {
                self.install_initialized_resources(*loaded);
                tracing::info!(
                    "Karukan resource initialization complete: {}",
                    self.model_name()
                );
            }
            Err(error) => {
                tracing::warn!("Resource initialization failed; keeping rule-based input: {error}");
            }
        }
    }
}

/// Build the configured model set without borrowing the live input engine.
/// The result can therefore cross a channel and be installed atomically.
fn load_model_converters(settings: &Settings) -> Result<Converters> {
    let strategy = settings.conversion.strategy;
    let n_threads = settings.conversion.n_threads;
    let mut loaded = Converters {
        romaji: RomajiConverter::new(),
        kanji: None,
        light_kanji: None,
        rewriters: RewriterChain::default_chain(),
    };

    match strategy {
        StrategyMode::Light => {
            let light_variant = resolve_variant_id(settings.conversion.light_model.as_deref())
                .context("invalid light_model settings")?;
            loaded.kanji = Some(
                create_converter(&light_variant, n_threads)
                    .context("failed to initialize light model")?,
            );
        }
        StrategyMode::Main => {
            let main_variant = resolve_variant_id(settings.conversion.model.as_deref())
                .context("invalid model settings")?;
            loaded.kanji = Some(
                create_converter(&main_variant, n_threads)
                    .context("failed to initialize main model")?,
            );
        }
        StrategyMode::Adaptive => {
            let main_variant = resolve_variant_id(settings.conversion.model.as_deref())
                .context("invalid model settings")?;
            loaded.kanji = Some(
                create_converter(&main_variant, n_threads)
                    .context("failed to initialize default model")?,
            );

            let configured_light = settings.conversion.light_model.clone();
            let light_variant = match resolve_variant_id(configured_light.as_deref()) {
                Ok(id) => id,
                Err(error) => {
                    tracing::warn!("Invalid light_model settings, using default: {error}");
                    karukan_engine::kanji::registry().default_model.clone()
                }
            };
            match create_converter(&light_variant, n_threads) {
                Ok(converter) => loaded.light_kanji = Some(converter),
                Err(error) => tracing::warn!(
                    "Failed to initialize beam model (light_model={configured_light:?}): {error}"
                ),
            }
        }
    }

    Ok(loaded)
}

impl InputMethodEngine {
    /// Initialize the kanji converter (call this early to avoid latency)
    /// Uses the default model from the registry.
    pub fn init_kanji_converter(&mut self) -> Result<()> {
        let default_id = karukan_engine::kanji::registry().default_model.clone();
        self.init_kanji_converter_with_model(&default_id, 0)
    }

    /// Initialize the kanji converter with a specific variant id
    pub fn init_kanji_converter_with_model(
        &mut self,
        variant_id: &str,
        n_threads: u32,
    ) -> Result<()> {
        if self.converters.kanji.is_none() {
            debug!("Initializing kanji converter with variant: {}", variant_id);
            let converter = create_converter(variant_id, n_threads)?;
            debug!(
                "Kanji converter initialized: {} (n_threads={})",
                converter.model_display_name(),
                threads_label(n_threads)
            );
            self.converters.kanji = Some(converter);
        }
        Ok(())
    }

    /// Initialize the light model for beam search (generates multiple candidates on Space conversion)
    pub fn init_light_kanji_converter(&mut self, variant_id: &str, n_threads: u32) -> Result<()> {
        if self.converters.light_kanji.is_none() {
            debug!(
                "Initializing light kanji converter with variant: {}",
                variant_id
            );
            let converter = create_converter(variant_id, n_threads)?;
            debug!(
                "Light kanji converter initialized: {} (n_threads={})",
                converter.model_display_name(),
                threads_label(n_threads)
            );
            self.converters.light_kanji = Some(converter);
        }
        Ok(())
    }

    /// Initialize the system dictionary for candidate lookup
    ///
    /// Uses `dict_path` from settings if specified, otherwise defaults to `data_dir/dict.bin`.
    /// If the file doesn't exist, the engine continues without a dictionary.
    pub fn init_system_dictionary(&mut self, dict_path: Option<&str>) {
        if self.dicts.system.is_some() {
            return;
        }

        let path = if let Some(p) = dict_path {
            std::path::PathBuf::from(p)
        } else if let Some(data_dir) = Settings::data_dir() {
            data_dir.join("dict.bin")
        } else {
            debug!("Could not determine data directory for system dictionary");
            return;
        };

        if !path.exists() {
            debug!("System dictionary not found at {:?}, skipping", path);
            return;
        }

        match Dictionary::load(&path) {
            Ok(dict) => {
                debug!("System dictionary loaded from {:?}", path);
                self.dicts.system = Some(dict);
            }
            Err(e) => {
                debug!("Failed to load system dictionary from {:?}: {}", path, e);
            }
        }
    }

    /// Apply a completed background dictionary update between key events.
    pub(super) fn poll_dictionary_update(&mut self) {
        let Some(receiver) = self.dictionary_update.as_ref() else {
            return;
        };
        let results = match receiver.try_recv() {
            Ok(first) => Some(
                std::iter::once(first)
                    .chain(receiver.try_iter())
                    .collect::<Vec<_>>(),
            ),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.dictionary_update = None;
                return;
            }
        };
        let Some(results) = results else {
            return;
        };

        for result in results {
            match result {
                Ok(update) => match update.outcome {
                    DictionaryUpdateOutcome::Updated { version, path } => {
                        if let Some(dictionary) = update.dictionary {
                            self.dicts.system = Some(dictionary);
                            tracing::info!(
                                "System dictionary hot-reloaded after update: {} ({})",
                                version,
                                path.display()
                            );
                        } else {
                            tracing::warn!(
                                "System dictionary update {version} completed without a loaded dictionary"
                            );
                        }
                    }
                    DictionaryUpdateOutcome::UpToDate { version } => {
                        debug!("System dictionary is up to date: {}", version);
                    }
                    DictionaryUpdateOutcome::Skipped { reason } => {
                        debug!("System dictionary update skipped: {}", reason);
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        "System dictionary update failed; keeping current dictionary: {error}"
                    );
                }
            }
        }
    }

    /// Initialize the learning cache from disk.
    ///
    /// Loads `~/.local/share/karukan-im/learning.tsv` if it exists.
    /// If the file doesn't exist, creates an empty in-memory cache.
    /// `config.max_surface_chars` caps the surface length `record` accepts;
    /// entries already on disk are loaded regardless (they can be removed
    /// with Ctrl+Delete or by eviction).
    pub fn init_learning_cache(&mut self, enabled: bool, config: LearningConfig) {
        if !enabled || self.learning.is_some() {
            return;
        }

        let cache = match Settings::learning_file() {
            Some(path) if path.exists() => match LearningCache::load(&path, config) {
                Ok(cache) => {
                    debug!(
                        "Learning cache loaded from {:?} ({} entries)",
                        path,
                        cache.entry_count()
                    );
                    cache
                }
                Err(e) => {
                    debug!("Failed to load learning cache from {:?}: {}", path, e);
                    LearningCache::new(config)
                }
            },
            Some(path) => {
                debug!("Learning cache not found at {:?}, starting empty", path);
                LearningCache::new(config)
            }
            None => {
                debug!("Could not determine learning cache path");
                LearningCache::new(config)
            }
        };
        self.learning = Some(cache);
    }

    /// Initialize user dictionaries by scanning the user dictionary directory.
    ///
    /// All files in the directory are loaded with `Dictionary::load_auto()`
    /// (auto-detects KRKN binary or Mozc TSV). Files are loaded in sorted
    /// order; earlier files have higher priority after merging.
    ///
    /// Default directory: `~/.local/share/karukan-im/user_dicts/`
    pub fn init_user_dictionaries(&mut self) {
        if self.dicts.user.is_some() {
            return;
        }

        let Some(dir) = Settings::user_dict_dir() else {
            debug!("Could not determine user dictionary directory");
            return;
        };

        if !dir.exists() {
            debug!(
                "User dictionary directory {:?} does not exist, skipping",
                dir
            );
            return;
        }

        let Ok(entries) = std::fs::read_dir(&dir) else {
            debug!("Failed to read user dictionary directory {:?}", dir);
            return;
        };
        let mut paths: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();

        if paths.is_empty() {
            debug!("No files in user dictionary directory {:?}", dir);
            return;
        }

        // Sort for deterministic load order (alphabetical)
        paths.sort();

        let mut dicts = Vec::new();
        for path in &paths {
            match Dictionary::load_auto(path) {
                Ok(dict) => {
                    debug!("User dictionary loaded from {:?}", path);
                    dicts.push(dict);
                }
                Err(e) => {
                    debug!("Failed to load user dictionary from {:?}: {}", path, e);
                }
            }
        }

        if dicts.is_empty() {
            return;
        }

        match Dictionary::merge(dicts) {
            Ok(Some(merged)) => {
                debug!(
                    "User dictionaries merged successfully ({} files from {:?})",
                    paths.len(),
                    dir
                );
                self.dicts.user = Some(merged);
            }
            Ok(None) => {}
            Err(e) => {
                debug!("Failed to merge user dictionaries: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod dictionary_update_tests {
    use std::path::PathBuf;
    use std::sync::mpsc;

    use tempfile::tempdir;

    use super::*;
    use crate::dictionary_update::BackgroundDictionaryUpdate;

    #[test]
    fn completed_background_update_is_hot_reloaded() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source.json");
        std::fs::write(
            &source,
            r#"[{"reading":"さいしん","candidates":[{"surface":"最新","score":1.0}]}]"#,
        )
        .unwrap();
        let dictionary = Dictionary::build_from_json(&source).unwrap();

        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok(BackgroundDictionaryUpdate {
                outcome: DictionaryUpdateOutcome::Updated {
                    version: "test-latest".to_string(),
                    path: PathBuf::from("dict.bin"),
                },
                dictionary: Some(dictionary),
            }))
            .unwrap();

        let mut engine = InputMethodEngine::new();
        engine.dictionary_update = Some(receiver);
        engine.poll_dictionary_update();

        let result = engine
            .dicts
            .system
            .as_ref()
            .unwrap()
            .exact_match_search("さいしん")
            .unwrap();
        assert_eq!(result.candidates[0].surface, "最新");
    }
}

#[cfg(test)]
mod resource_initialization_tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn completed_async_resources_are_installed_between_keys() {
        let (sender, receiver) = mpsc::channel();
        let mut engine = InputMethodEngine::new();
        engine.resource_initialization = Some(receiver);
        assert_eq!(engine.model_name(), "initializing");
        engine.process_key(&KeyEvent::press(Keysym::KEY_K));
        engine.process_key(&KeyEvent::press(Keysym::KEY_A));
        assert_eq!(engine.input_buf.text, "か");

        sender.send(Ok(Box::new(InputMethodEngine::new()))).unwrap();
        engine.poll_resource_initialization();

        assert!(engine.resource_initialization.is_none());
        assert_eq!(engine.model_name(), "unknown");
        assert_eq!(engine.input_buf.text, "か");
        assert!(matches!(engine.state(), InputState::Composing { .. }));
    }

    #[test]
    fn model_load_failure_does_not_block_rule_based_input() {
        let mut settings = Settings::default();
        settings.conversion.strategy = StrategyMode::Main;
        settings.conversion.model = Some("not-a-real-model".to_string());
        settings.conversion.live_conversion = false;
        settings.learning.enabled = false;
        settings.dictionary_update.enabled = false;

        let config = EngineConfig::from_settings(&settings);
        let mut engine = InputMethodEngine::with_config(config);
        engine.init_from_settings_async(&settings).unwrap();

        engine.process_key(&KeyEvent::press(Keysym::KEY_K));
        let result = engine.process_key(&KeyEvent::press(Keysym::KEY_A));

        assert!(result.consumed);
        assert_eq!(engine.input_buf.text, "か");
    }
}
