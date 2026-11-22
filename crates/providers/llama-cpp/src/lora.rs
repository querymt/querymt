//! LoRA adapter support for the llama-cpp provider: adapter path resolution,
//! loading adapters onto a model, the [`Models`] view that generation entry
//! points take, creation of target contexts with the adapter applied, and the
//! typed errors for all of these.

use crate::config::LoraAdapterConfig;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::model::{LlamaLoraAdapter, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::{LlamaLoraAdapterInitError, LlamaLoraAdapterSetError};
use querymt::error::LLMError;
use querymt_provider_common::{
    HfFileRef, ModelRef, ModelRefError, download_hf_file_sync, parse_model_ref, terminal_progress,
};
use std::error::Error as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

/// Number of adapters a provider accepts. `llama-cpp-2` replaces the whole
/// adapter set on every `lora_adapter_set` call, so only one can be active.
const MAX_ADAPTERS: usize = 1;

/// Failures while validating, loading or applying LoRA adapters.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LoraError {
    #[error("`lora` lists {count} adapters, but only one LoRA adapter is supported")]
    TooMany { count: usize },
    #[error("`lora[{index}].path` {path:?} is not a valid model reference: {reason}")]
    InvalidRef {
        index: usize,
        path: String,
        reason: String,
    },
    #[error("`lora[{index}].path` {path:?} does not exist")]
    NotFound { index: usize, path: PathBuf },
    #[error("failed to download `lora[{index}].path` {path:?}: {reason}")]
    Download {
        index: usize,
        path: String,
        reason: String,
    },
    #[error(
        "`lora[{index}].path` {path:?} could not be loaded onto this base model \
         (usually the file is not a LoRA adapter for it: different architecture or \
         tensor shapes, or not an adapter at all; otherwise llama.cpp ran out of memory)"
    )]
    Incompatible {
        index: usize,
        path: PathBuf,
        #[source]
        source: LlamaLoraAdapterInitError,
    },
    #[error("failed to apply `lora[{index}]` to the generation context")]
    Apply {
        index: usize,
        #[source]
        source: LlamaLoraAdapterSetError,
    },
    /// The loaded adapters and the generation model or configuration disagree.
    /// Never caused by user input: it means the provider wired them wrongly.
    #[error("LoRA adapters do not match the generation model: {reason}")]
    Inconsistent { reason: &'static str },
}

impl LoraError {
    /// The error message followed by every `source()` in the chain.
    fn message_with_sources(&self) -> String {
        let mut message = self.to_string();
        let mut source = self.source();
        while let Some(err) = source {
            message.push_str(": ");
            message.push_str(&err.to_string());
            source = err.source();
        }
        message
    }
}

impl From<LoraError> for LLMError {
    fn from(err: LoraError) -> Self {
        let message = err.message_with_sources();
        match err {
            // Network failures are retryable, like a failed base-model download.
            LoraError::Apply { .. }
            | LoraError::Download { .. }
            | LoraError::Inconsistent { .. } => Self::ProviderError(message),
            _ => Self::InvalidRequest(message),
        }
    }
}

/// One adapter loaded onto the base model.
pub(crate) struct LoadedLora {
    index: usize,
    path: PathBuf,
    /// `lora_adapter_set` takes `&mut` but only reads the pointer. The lock is
    /// held for that one synchronous call, never across an `.await`.
    adapter: Mutex<LlamaLoraAdapter>,
}

/// The adapters loaded onto one base model.
///
/// Holds a strong reference to that model, so an adapter can never outlive it
/// no matter how the set is cloned or moved between threads.
#[derive(Default)]
pub(crate) struct LoraSet {
    // Field order is drop order: adapters before the model.
    adapters: Box<[LoadedLora]>,
    model: Option<Arc<LlamaModel>>,
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LoraSet>();
};

impl LoraSet {
    /// Load one adapter per path onto `model`, then share the model.
    ///
    /// This is the only constructor: the adapters are created from `model`
    /// and bound to the `Arc` it ends up in within one step, so a set can never
    /// be paired with a different model. (`lora_adapter_init` needs `&mut`, which
    /// is why the model is taken by value and wrapped here.)
    ///
    /// # Errors
    ///
    /// [`LoraError::Incompatible`] when llama.cpp cannot load a file onto `model`.
    pub(crate) fn load(
        mut model: LlamaModel,
        paths: &[PathBuf],
    ) -> Result<(Arc<LlamaModel>, Self), LoraError> {
        let adapters = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let adapter =
                    model
                        .lora_adapter_init(path)
                        .map_err(|source| LoraError::Incompatible {
                            index,
                            path: path.clone(),
                            source,
                        })?;
                log::info!("Loaded LoRA adapter lora[{index}]: {}", path.display());
                Ok(LoadedLora {
                    index,
                    path: path.clone(),
                    adapter: Mutex::new(adapter),
                })
            })
            .collect::<Result<Box<[_]>, LoraError>>()?;
        let model = Arc::new(model);
        let set = Self {
            adapters,
            model: Some(Arc::clone(&model)),
        };
        Ok((model, set))
    }

    /// Number of loaded adapters.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.adapters.len()
    }

    /// Apply each loaded adapter to `ctx` at the scale configured at the same
    /// index in `cfg`. Disabled adapters (`scale == 0`) are skipped, so the
    /// context then behaves exactly like the base model.
    ///
    /// # Errors
    ///
    /// [`LoraError::Apply`] when llama.cpp rejects the adapter, and
    /// [`LoraError::Inconsistent`] when `cfg` asks for an adapter that is not
    /// loaded or `model` is not the model the adapters belong to. Both mean a
    /// wiring bug, and failing beats silently generating with the base model.
    pub(crate) fn apply(
        &self,
        model: &Arc<LlamaModel>,
        ctx: &mut LlamaContext<'_>,
        cfg: &[LoraAdapterConfig],
    ) -> Result<(), LoraError> {
        if !self.adapters.is_empty()
            && !self
                .model
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, model))
        {
            return Err(LoraError::Inconsistent {
                reason: "the context was created on a different model than the adapters",
            });
        }
        for (index, scale) in active_scales(cfg) {
            let Some(loaded) = self.adapters.get(index) else {
                return Err(LoraError::Inconsistent {
                    reason: "an adapter in the configuration is not loaded",
                });
            };
            let mut adapter = loaded
                .adapter
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            ctx.lora_adapter_set(&mut adapter, scale)
                .map_err(|source| LoraError::Apply {
                    index: loaded.index,
                    source,
                })?;
            log::debug!(
                "Applied LoRA adapter lora[{}] {} at scale {scale}",
                loaded.index,
                loaded.path.display()
            );
        }
        Ok(())
    }
}

/// Borrowed view of the models a generation runs on.
///
/// `Copy`, so generation entry points take one argument instead of a model
/// pair, and every target context can be built through the same path.
#[derive(Clone, Copy)]
pub(crate) struct Models<'a> {
    pub target: &'a Arc<LlamaModel>,
    pub draft: Option<&'a Arc<LlamaModel>>,
    pub loras: &'a LoraSet,
}

/// Create a generation context on the target model and apply the configured
/// adapters to it. The only way the provider creates a target context.
///
/// Context-creation errors are returned as-is so each call site keeps its own
/// memory-estimate message (see [`TargetContextError::into_llm_error`]).
///
/// # Errors
///
/// [`TargetContextError::Create`] when llama.cpp cannot create the context,
/// [`TargetContextError::Lora`] when an adapter cannot be applied.
pub(crate) fn new_target_context<'m>(
    models: Models<'m>,
    backend: &LlamaBackend,
    params: LlamaContextParams,
    samplers: Option<LlamaSampler>,
    lora_cfg: &[LoraAdapterConfig],
) -> Result<LlamaContext<'m>, TargetContextError> {
    let mut ctx = match samplers {
        Some(sampler) => models
            .target
            .new_context_with_samplers(backend, params, [(0, sampler)]),
        None => models.target.new_context(backend, params),
    }
    .map_err(TargetContextError::Create)?;
    models
        .loras
        .apply(models.target, &mut ctx, lora_cfg)
        .map_err(TargetContextError::Lora)?;
    Ok(ctx)
}

/// Why [`new_target_context`] failed.
#[derive(Debug)]
pub(crate) enum TargetContextError {
    /// llama.cpp could not create the context.
    Create(llama_cpp_2::LlamaContextLoadError),
    /// An adapter could not be applied.
    Lora(LoraError),
}

impl TargetContextError {
    /// Convert to [`LLMError`], rendering a creation failure with `create`.
    pub(crate) fn into_llm_error(
        self,
        create: impl FnOnce(llama_cpp_2::LlamaContextLoadError) -> LLMError,
    ) -> LLMError {
        match self {
            Self::Create(err) => create(err),
            Self::Lora(err) => err.into(),
        }
    }
}

/// The `(index, scale)` pairs that must be applied: disabled adapters
/// (`scale == 0`) are skipped so they match the base model exactly.
pub(crate) fn active_scales(
    adapters: &[LoraAdapterConfig],
) -> impl Iterator<Item = (usize, f32)> + '_ {
    adapters
        .iter()
        .enumerate()
        .filter(|(_, adapter)| !adapter.scale.is_disabled())
        .map(|(index, adapter)| (index, adapter.scale.get()))
}

/// A `lora` entry whose reference parsed but is not yet a file on disk.
enum Located {
    Local(PathBuf),
    Hf(HfFileRef),
}

/// `lora` entries that passed every check needing no network. Call
/// [`Self::download`] to fetch Hugging Face entries and get the local paths.
pub(crate) struct LocatedAdapters {
    /// The configured `path` strings (for error messages) and what they mean.
    entries: Vec<(String, Located)>,
}

/// Check every `lora` entry without touching the network: count, reference
/// syntax, and existence of local files.
///
/// The provider runs this before it resolves or downloads the base model, so a
/// bad adapter fails before any model work.
///
/// # Errors
///
/// Returns [`LoraError`] naming the offending `lora[<i>]` entry.
pub(crate) fn locate_lora_adapters(
    adapters: &[LoraAdapterConfig],
) -> Result<LocatedAdapters, LoraError> {
    if adapters.len() > MAX_ADAPTERS {
        return Err(LoraError::TooMany {
            count: adapters.len(),
        });
    }
    let entries = adapters
        .iter()
        .enumerate()
        .map(|(index, adapter)| Ok((adapter.path.clone(), locate(index, &adapter.path)?)))
        .collect::<Result<Vec<_>, LoraError>>()?;
    Ok(LocatedAdapters { entries })
}

impl LocatedAdapters {
    /// Download Hugging Face entries and return one local path per entry.
    ///
    /// # Errors
    ///
    /// [`LoraError::Download`] when a download fails, [`LoraError::InvalidRef`]
    /// when a resolved path cannot be handed to llama.cpp (not UTF-8, or it
    /// contains a NUL byte).
    pub(crate) fn download(self) -> Result<Box<[PathBuf]>, LoraError> {
        self.entries
            .into_iter()
            .enumerate()
            .map(|(index, (raw, located))| {
                let path = match located {
                    Located::Local(path) => path,
                    Located::Hf(file) => {
                        download_hf_file_sync(&file, terminal_progress(&file.file)).map_err(
                            |err| match err {
                                ModelRefError::Invalid(reason) => LoraError::InvalidRef {
                                    index,
                                    path: raw.clone(),
                                    reason,
                                },
                                ModelRefError::Download(reason) => LoraError::Download {
                                    index,
                                    path: raw.clone(),
                                    reason,
                                },
                            },
                        )?
                    }
                };
                match path.to_str() {
                    Some(text) if !text.contains('\0') => Ok(path),
                    _ => Err(LoraError::InvalidRef {
                        index,
                        path: raw,
                        reason: "the resolved path is not valid UTF-8 or contains a NUL byte"
                            .to_owned(),
                    }),
                }
            })
            .collect()
    }
}

/// Validate every `lora` entry and resolve it to a local file
/// ([`locate_lora_adapters`] followed by [`LocatedAdapters::download`]).
///
/// # Errors
///
/// Returns [`LoraError`] naming the offending `lora[<i>]` entry.
#[cfg(test)]
pub(crate) fn resolve_lora_paths(
    adapters: &[LoraAdapterConfig],
) -> Result<Box<[PathBuf]>, LoraError> {
    locate_lora_adapters(adapters)?.download()
}

/// Parse one reference and check that a local path exists. Needs no network.
fn locate(index: usize, raw: &str) -> Result<Located, LoraError> {
    let invalid = |reason: String| LoraError::InvalidRef {
        index,
        path: raw.to_owned(),
        reason,
    };
    match parse_model_ref(raw).map_err(|err| invalid(err.to_string()))? {
        ModelRef::LocalPath(path) if path.exists() => Ok(Located::Local(path)),
        ModelRef::LocalPath(path) => Err(LoraError::NotFound { index, path }),
        ModelRef::Hf(model) => Ok(Located::Hf(HfFileRef::from(&model))),
        ModelRef::HfRepo(repo) => Err(invalid(format!(
            "a Hugging Face repo needs a selector: {repo}:<selector>"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LoraScale;

    fn adapters(paths: &[&str]) -> Vec<LoraAdapterConfig> {
        paths.iter().map(|p| LoraAdapterConfig::new(*p)).collect()
    }

    #[test]
    fn lora_error_message_includes_source_chain() {
        let err = LoraError::Incompatible {
            index: 0,
            path: PathBuf::from("/models/a.gguf"),
            source: LlamaLoraAdapterInitError::NullResult,
        };
        let LLMError::InvalidRequest(message) = LLMError::from(err) else {
            panic!("incompatible adapters map to InvalidRequest");
        };
        assert!(message.contains("lora[0].path"), "{message}");
        assert!(message.contains("/models/a.gguf"), "{message}");
        assert!(message.contains("null result from llama cpp"), "{message}");

        let apply = LoraError::Apply {
            index: 0,
            source: LlamaLoraAdapterSetError::ErrorResult(-1),
        };
        assert!(matches!(LLMError::from(apply), LLMError::ProviderError(_)));
    }

    #[test]
    fn download_and_wiring_failures_are_provider_errors() {
        let download = LoraError::Download {
            index: 0,
            path: "owner/repo:a.gguf".to_owned(),
            reason: "timeout".to_owned(),
        };
        assert!(matches!(
            LLMError::from(download),
            LLMError::ProviderError(_)
        ));
        let wiring = LoraError::Inconsistent { reason: "x" };
        assert!(matches!(LLMError::from(wiring), LLMError::ProviderError(_)));
        let invalid = LoraError::InvalidRef {
            index: 0,
            path: "p".to_owned(),
            reason: "r".to_owned(),
        };
        assert!(matches!(
            LLMError::from(invalid),
            LLMError::InvalidRequest(_)
        ));
    }

    /// Multimodal and speculative generation have no small public fixture with
    /// a matching adapter, so this guard checks that every target context goes
    /// through `new_target_context`, and that only the MTP draft context is
    /// created directly. Every `src` file is listed by hand: add new files
    /// that touch llama.cpp contexts here (the test below fails when a `.rs`
    /// file exists on disk that is not listed).
    #[test]
    fn adapter_applies_to_tool_call_multimodal_and_speculative_generation() {
        let sources = [
            ("generation.rs", include_str!("generation.rs")),
            ("mtp.rs", include_str!("mtp.rs")),
            ("multimodal.rs", include_str!("multimodal.rs")),
            ("provider.rs", include_str!("provider.rs")),
            ("context.rs", include_str!("context.rs")),
            ("tools/generation.rs", include_str!("tools/generation.rs")),
            ("tools/prefill.rs", include_str!("tools/prefill.rs")),
            ("tools/sampler.rs", include_str!("tools/sampler.rs")),
            ("tools/streaming.rs", include_str!("tools/streaming.rs")),
        ];
        let count = |file: &str, needle: &str| {
            sources
                .iter()
                .find(|(name, _)| *name == file)
                .map_or(0, |(_, text)| text.matches(needle).count())
        };

        // Every `new_context*` constructor of `LlamaModel`, however named.
        for (name, text) in &sources {
            let direct = text.matches(".new_context").count();
            let allowed = usize::from(*name == "mtp.rs");
            assert_eq!(direct, allowed, "{name} creates a context without LoRA");
        }
        assert_eq!(count("generation.rs", "new_target_context("), 2);
        assert_eq!(count("tools/prefill.rs", "new_target_context("), 2);
        assert_eq!(count("mtp.rs", "new_target_context("), 1);

        // Other source files must not create contexts either.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut unlisted = Vec::new();
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path
                    .strip_prefix(&src)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let listed = sources.iter().any(|(name, _)| *name == rel);
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                if !listed && rel != "lora.rs" && text.contains(".new_context") {
                    unlisted.push(rel);
                }
            }
        }
        assert!(
            unlisted.is_empty(),
            "unlisted files create contexts: {unlisted:?}"
        );
    }

    #[test]
    fn active_scales_skips_disabled_adapters() {
        let configured = [
            LoraAdapterConfig::new("a").with_scale(LoraScale::new(0.0).unwrap()),
            LoraAdapterConfig::new("b").with_scale(LoraScale::new(-0.0).unwrap()),
            LoraAdapterConfig::new("c").with_scale(LoraScale::new(0.5).unwrap()),
        ];
        let active: Vec<_> = active_scales(&configured).collect();
        assert_eq!(active, [(2, 0.5)]);
    }

    #[test]
    fn empty_lora_resolves_to_no_paths() {
        assert!(resolve_lora_paths(&[]).unwrap().is_empty());
    }

    #[test]
    fn existing_adapter_file_resolves_to_its_path() {
        let file =
            std::env::temp_dir().join(format!("qmt-lora-exists-{}.gguf", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let paths = resolve_lora_paths(&adapters(&[file.to_str().unwrap()])).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert_eq!(&*paths, [file]);
    }

    #[test]
    fn missing_adapter_file_rejected() {
        let err = resolve_lora_paths(&adapters(&["/nonexistent/style.gguf"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("lora[0].path"), "{err}");
        assert!(err.contains("/nonexistent/style.gguf"), "{err}");
    }

    #[test]
    fn hugging_face_adapter_without_selector_rejected() {
        let err = resolve_lora_paths(&adapters(&["owner/repo"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("lora[0].path"), "{err}");
        assert!(err.contains("selector"), "{err}");
    }

    #[test]
    fn locating_adapters_needs_no_network() {
        // A Hugging Face adapter is accepted by the offline step; only
        // `download` would touch the network.
        let located = locate_lora_adapters(&adapters(&["owner/repo:adapter.gguf"]));
        assert!(located.is_ok());
        let err = locate_lora_adapters(&adapters(&["/nonexistent/a.gguf"])).err();
        assert!(matches!(err, Some(LoraError::NotFound { index: 0, .. })));
    }

    #[test]
    fn more_than_one_adapter_rejected() {
        let err = resolve_lora_paths(&adapters(&["/nonexistent/a.gguf", "/nonexistent/b.gguf"]))
            .unwrap_err();
        assert!(matches!(err, LoraError::TooMany { count: 2 }));
        let message = err.to_string();
        assert!(message.contains("`lora`"), "{message}");
        assert!(message.contains("only one"), "{message}");
    }
}

/// Live checks with a real base model and adapter. They are `#[ignore]` and
/// skip unless `QMT_RUN_LIVE_LLAMA_LORA=1` (downloads also need
/// `QMT_LLAMA_VOCAB_DOWNLOAD=1`).
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::config::LlamaCppConfig;
    use crate::provider::{CachedModel, LlamaCppProvider};
    use crate::test_vocab::{LORA_ADAPTER, LORA_BASE_MODEL, VocabError, ensure_full_model};
    use futures::StreamExt;
    use querymt::chat::{ChatCall, ChatMessage, ChatProvider, StreamChunk};
    use querymt::completion::{CompletionProvider, CompletionRequest};

    const PROMPT: &str = "Look in thy glass";
    /// The fixture has no chat template, so chat tests supply a minimal one.
    const TEMPLATE: &str = "{% for m in messages %}{{ m.content }}{% endfor %}";

    struct Fixture {
        base: String,
        adapter: String,
    }

    /// `None` (with a `SKIPPED` note) unless the live check is enabled and
    /// both files are available.
    fn fixture() -> Option<Fixture> {
        if std::env::var_os("QMT_RUN_LIVE_LLAMA_LORA").is_none() {
            eprintln!("SKIPPED: set QMT_RUN_LIVE_LLAMA_LORA=1 to run the live LoRA checks");
            return None;
        }
        let fetch = |file| match ensure_full_model(file) {
            Ok(path) => Some(path.to_string_lossy().into_owned()),
            Err(VocabError::Unavailable(why)) => {
                eprintln!("SKIPPED: LoRA fixture unavailable: {why}");
                None
            }
            Err(changed) => panic!("{changed}"),
        };
        Some(Fixture {
            base: fetch(&LORA_BASE_MODEL)?,
            adapter: fetch(&LORA_ADAPTER)?,
        })
    }

    /// Greedy, seeded, CPU-only config; `scale = None` means no `lora` key.
    fn config_json(fx: &Fixture, scale: Option<f32>) -> serde_json::Value {
        let mut cfg = serde_json::json!({
            "model": fx.base,
            "temperature": 0.0,
            "seed": 42,
            "max_tokens": 32,
            "n_ctx": 512,
            "n_gpu_layers": 0,
            "chat_template": TEMPLATE,
        });
        if let Some(scale) = scale {
            cfg["lora"] = serde_json::json!([{ "path": fx.adapter, "scale": scale }]);
        }
        cfg
    }

    fn config(fx: &Fixture, scale: Option<f32>) -> LlamaCppConfig {
        serde_json::from_value(config_json(fx, scale)).unwrap()
    }

    fn complete(provider: &LlamaCppProvider) -> String {
        let request = CompletionRequest {
            prompt: PROMPT.to_owned(),
            suffix: None,
            max_tokens: None,
            temperature: None,
        };
        futures::executor::block_on(provider.complete(&request))
            .expect("completion")
            .text
    }

    fn stream_text(provider: &LlamaCppProvider) -> String {
        let messages = [ChatMessage::user().text(PROMPT).build()];
        futures::executor::block_on(async {
            let mut stream = provider
                .chat_stream(&ChatCall::new(&messages))
                .await
                .expect("stream");
            let mut text = String::new();
            while let Some(chunk) = stream.next().await {
                if let StreamChunk::Text(piece) = chunk.expect("chunk") {
                    text.push_str(&piece);
                }
            }
            text
        })
    }

    #[test]
    #[ignore = "live: set QMT_RUN_LIVE_LLAMA_LORA=1 and use --run-ignored"]
    fn lora_live_fixtures_load() {
        let Some(fx) = fixture() else { return };
        let provider = LlamaCppProvider::new(config(&fx, Some(1.0))).expect("load with adapter");
        assert_eq!(provider.loras.len(), 1);
    }

    #[test]
    #[ignore = "live: set QMT_RUN_LIVE_LLAMA_LORA=1 and use --run-ignored"]
    fn adapter_changes_generated_text_lora_live() {
        let Some(fx) = fixture() else { return };
        let base = complete(&LlamaCppProvider::new(config(&fx, None)).unwrap());
        let adapted = complete(&LlamaCppProvider::new(config(&fx, Some(1.0))).unwrap());
        assert_ne!(base, adapted, "the adapter must change the output");
        // `glass` is left out: it is in the prompt, so it proves nothing.
        let shakespearean = regex::Regex::new("(eye|love|sun)").unwrap();
        assert!(shakespearean.is_match(&adapted), "{adapted:?}");
        assert!(
            !shakespearean.is_match(&base),
            "base output already matches: {base:?}"
        );
    }

    #[test]
    #[ignore = "live: set QMT_RUN_LIVE_LLAMA_LORA=1 and use --run-ignored"]
    fn zero_scale_matches_the_base_model_lora_live() {
        let Some(fx) = fixture() else { return };
        let base = complete(&LlamaCppProvider::new(config(&fx, None)).unwrap());
        let zero = complete(&LlamaCppProvider::new(config(&fx, Some(0.0))).unwrap());
        assert_eq!(base, zero);
    }

    #[test]
    #[ignore = "live: set QMT_RUN_LIVE_LLAMA_LORA=1 and use --run-ignored"]
    fn adapter_applies_to_streaming_output_lora_live() {
        let Some(fx) = fixture() else { return };
        let base = stream_text(&LlamaCppProvider::new(config(&fx, None)).unwrap());
        let zero = stream_text(&LlamaCppProvider::new(config(&fx, Some(0.0))).unwrap());
        let full = stream_text(&LlamaCppProvider::new(config(&fx, Some(1.0))).unwrap());
        assert_ne!(full, zero, "scale 1.0 must differ from scale 0");
        assert_eq!(zero, base, "scale 0 must match the base model");
    }

    #[test]
    #[ignore = "live: set QMT_RUN_LIVE_LLAMA_LORA=1 and use --run-ignored"]
    fn adapter_applies_to_tool_call_generation_lora_live() {
        use crate::multimodal::MediaInputs;
        use crate::tools::prefill::prefill_for_tool_generation;

        let Some(fx) = fixture() else { return };
        let provider = LlamaCppProvider::new(config(&fx, Some(1.0))).unwrap();
        let empty = LoraSet::default();
        let last_logits = |models: Models<'_>, cfg: &LlamaCppConfig| {
            let state = prefill_for_tool_generation(models, cfg, PROMPT, 4, MediaInputs::default())
                .expect("prefill");
            state.ctx.get_logits_ith(state.n_cur - 1).to_vec()
        };

        let full = last_logits(provider.models(), &config(&fx, Some(1.0)));
        let zero = last_logits(provider.models(), &config(&fx, Some(0.0)));
        let base = last_logits(
            Models {
                loras: &empty,
                ..provider.models()
            },
            &config(&fx, None),
        );
        assert_ne!(full, zero, "scale 1.0 must change the logits");
        assert_eq!(zero, base, "scale 0 must match an empty adapter set");
    }

    #[test]
    #[ignore = "live: set QMT_RUN_LIVE_LLAMA_LORA=1 and use --run-ignored"]
    fn incompatible_adapter_rejected_lora_live() {
        let Some(fx) = fixture() else { return };
        let factory = crate::create_factory();

        let mut bad = config_json(&fx, None);
        bad["lora"] = serde_json::json!([{ "path": fx.base }]);
        let err = factory
            .from_config(&bad.to_string())
            .err()
            .expect("the base model is not an adapter")
            .to_string();
        assert!(err.contains("lora[0].path"), "{err}");
        assert!(err.contains(&fx.base), "{err}");

        factory
            .from_config(&config_json(&fx, Some(1.0)).to_string())
            .expect("the process survives and accepts a valid adapter");
    }

    #[test]
    #[ignore = "live: set QMT_RUN_LIVE_LLAMA_LORA=1 and use --run-ignored"]
    fn changed_scale_reuses_the_loaded_model_lora_live() {
        let Some(fx) = fixture() else { return };
        let cache = std::sync::Mutex::new(None::<CachedModel>);
        let full = LlamaCppProvider::new_with_cache(config(&fx, Some(1.0)), &cache).unwrap();
        let half = LlamaCppProvider::new_with_cache(config(&fx, Some(0.5)), &cache).unwrap();
        assert!(Arc::ptr_eq(&full.loras, &half.loras));
        assert!(Arc::ptr_eq(&full.model, &half.model));
        assert_ne!(complete(&full), complete(&half));
    }

    #[test]
    #[ignore = "live: set QMT_RUN_LIVE_LLAMA_LORA=1 and use --run-ignored"]
    fn changed_adapter_list_reloads_the_model_lora_live() {
        let Some(fx) = fixture() else { return };
        let cache = std::sync::Mutex::new(None::<CachedModel>);
        let with = LlamaCppProvider::new_with_cache(config(&fx, Some(1.0)), &cache).unwrap();
        let without = LlamaCppProvider::new_with_cache(config(&fx, None), &cache).unwrap();
        assert!(!Arc::ptr_eq(&with.model, &without.model));
        assert_eq!(without.loras.len(), 0);
    }
}
