//! Test support: real tokenizers without real weights.
//!
//! Every tool-call format is checked on the vocabulary of a model that uses
//! it. Vocab-only GGUFs are fetched from pinned sources and cached:
//!
//! * llama.cpp `models/ggml-vocab-*.gguf` files at a fixed commit, fetched
//!   whole (6-20 MB);
//! * Hugging Face model repositories at a fixed revision. A GGUF stores all
//!   metadata (including the tokenizer) before the tensor data, so only a
//!   `Range` prefix is fetched, cut at the end of the key/value section and
//!   rewritten with tensor count 0. llama.cpp loads that with `vocab_only`.
//!
//! The cache is `$TMPDIR/qmt-llama-vocab`, or `QMT_LLAMA_VOCAB_DIR`.
//! Network trouble yields [`VocabError::Unavailable`] (tests skip); a pinned
//! file whose metadata no longer parses or fits yields
//! [`VocabError::PinnedFileChanged`] (tests fail).

use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::model::params::LlamaModelParams;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// llama.cpp commit that added the vocab files.
const LLAMA_CPP_COMMIT: &str = "42532afff43910e619a650c1704525b3acbbec5a";

const MIB: u64 = 1024 * 1024;

/// Tokenizers the grammar tests run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vocab {
    Qwen35,
    Gemma4,
    Lfm2,
    FunctionGemma,
}

/// A model file in a Hugging Face repository at a pinned revision.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HfFile {
    pub repo: &'static str,
    pub rev: &'static str,
    pub file: &'static str,
}

impl fmt::Display for HfFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{} {}", self.repo, self.rev, self.file)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Source {
    LlamaCpp { file: &'static str },
    HuggingFace(HfFile),
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LlamaCpp { file } => write!(f, "llama.cpp@{LLAMA_CPP_COMMIT} models/{file}"),
            Self::HuggingFace(hf) => hf.fmt(f),
        }
    }
}

impl Source {
    /// File name in the cache directory.
    fn cache_name(&self) -> String {
        match self {
            Self::LlamaCpp { file } => (*file).to_string(),
            Self::HuggingFace(hf) => {
                format!(
                    "{}--{}--{}.vocab",
                    hf.repo.replace('/', "--"),
                    &hf.rev[..12],
                    hf.file
                )
            }
        }
    }
}

impl Vocab {
    pub(crate) const ALL: [Self; 4] = [Self::Qwen35, Self::Gemma4, Self::Lfm2, Self::FunctionGemma];

    pub(crate) fn source(self) -> Source {
        match self {
            Self::Qwen35 => Source::LlamaCpp {
                file: "ggml-vocab-qwen35.gguf",
            },
            Self::Gemma4 => Source::LlamaCpp {
                file: "ggml-vocab-gemma-4.gguf",
            },
            Self::Lfm2 => Source::HuggingFace(HfFile {
                repo: "LiquidAI/LFM2-350M-GGUF",
                rev: "8fdc9d526b7ed346b19257551b05816c7912ecc2",
                file: "LFM2-350M-Q4_0.gguf",
            }),
            Self::FunctionGemma => Source::HuggingFace(HfFile {
                repo: "ggml-org/functiongemma-270m-it-GGUF",
                rev: "2566ce14aedfc14fdd0de955ba67346425e67126",
                file: "functiongemma-270m-it-q8_0.gguf",
            }),
        }
    }

    pub(crate) fn n_vocab(self) -> i32 {
        match self {
            Self::Qwen35 => 151_936,
            Self::Gemma4 | Self::FunctionGemma => 262_144,
            Self::Lfm2 => 65_536,
        }
    }
}

/// Where the sources are fetched from; tests point this at fixtures or at
/// an unreachable address.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hosts<'a> {
    pub llama_cpp: &'a str,
    pub hugging_face: &'a str,
}

impl Hosts<'static> {
    pub(crate) const PINNED: Self = Self {
        llama_cpp: "https://raw.githubusercontent.com/ggml-org/llama.cpp",
        hugging_face: "https://huggingface.co",
    };
}

/// Prefix sizes for Hugging Face fetches: start at `initial`, double until
/// the key/value section fits, give up at `max`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub initial: u64,
    pub max: u64,
}

impl Limits {
    pub(crate) const PINNED: Self = Self {
        initial: 8 * MIB,
        max: 64 * MIB,
    };
}

#[derive(Debug)]
pub(crate) enum VocabError {
    /// Could not be fetched (offline, rate limited, no curl): skip.
    Unavailable(String),
    /// Fetched, but not what the pin promised: fail.
    PinnedFileChanged(String),
}

impl fmt::Display for VocabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(m) | Self::PinnedFileChanged(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for VocabError {}

/// Fetch `url`; with `Some(n)` only the first `n` bytes.
pub(crate) type Fetch<'a> = dyn FnMut(&str, Option<u64>) -> Result<Vec<u8>, String> + 'a;

/// Default transport: `curl` (also serves `file://` fixtures).
pub(crate) fn curl(url: &str, len: Option<u64>) -> Result<Vec<u8>, String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-fsSL", "--retry", "3", "--connect-timeout", "15"]);
    if let Some(n) = len {
        cmd.arg("-r").arg(format!("0-{}", n.saturating_sub(1)));
    }
    let out = cmd
        .arg(url)
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "curl {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

pub(crate) fn cache_dir() -> PathBuf {
    std::env::var_os("QMT_LLAMA_VOCAB_DIR").map_or_else(
        || std::env::temp_dir().join("qmt-llama-vocab"),
        PathBuf::from,
    )
}

/// Return the cached vocab-only GGUF for `source`, fetching it if missing.
pub(crate) fn ensure(
    source: &Source,
    dir: &Path,
    hosts: Hosts<'_>,
    limits: Limits,
    fetch: &mut Fetch<'_>,
) -> Result<PathBuf, VocabError> {
    let path = dir.join(source.cache_name());
    if path.metadata().is_ok_and(|m| m.len() > 0) {
        return Ok(path);
    }
    let unavailable = |e: String| VocabError::Unavailable(format!("{source}: {e}"));
    let bytes = match source {
        Source::LlamaCpp { file } => {
            let url = format!("{}/{LLAMA_CPP_COMMIT}/models/{file}", hosts.llama_cpp);
            fetch(&url, None).map_err(unavailable)?
        }
        Source::HuggingFace(hf) => fetch_prefix(hf, hosts.hugging_face, limits, fetch)?,
    };
    std::fs::create_dir_all(dir).map_err(|e| unavailable(e.to_string()))?;
    // Unique per process and thread so concurrent runs never share it.
    let thread: String = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let part = path.with_extension(format!("{}.{thread}.part", std::process::id()));
    std::fs::write(&part, &bytes).map_err(|e| unavailable(e.to_string()))?;
    if std::fs::rename(&part, &path).is_err() {
        // Another runner may have finished first; accept its file.
        let _ = std::fs::remove_file(&part);
        if !path.metadata().is_ok_and(|m| m.len() > 0) {
            return Err(unavailable("cannot store the vocabulary".into()));
        }
    }
    Ok(path)
}

/// Metadata prefix of a Hugging Face GGUF, rewritten with tensor count 0.
fn fetch_prefix(
    hf: &HfFile,
    base: &str,
    limits: Limits,
    fetch: &mut Fetch<'_>,
) -> Result<Vec<u8>, VocabError> {
    let url = format!("{base}/{}/resolve/{}/{}", hf.repo, hf.rev, hf.file);
    let changed = |why: String| VocabError::PinnedFileChanged(format!("{hf}: {why}"));
    let mut len = limits.initial;
    loop {
        let buf =
            fetch(&url, Some(len)).map_err(|e| VocabError::Unavailable(format!("{hf}: {e}")))?;
        match kv_section_end(&buf) {
            Ok(Section::End(end)) => {
                let mut prefix = buf[..end].to_vec();
                prefix[8..16].fill(0);
                return Ok(prefix);
            }
            Ok(Section::NeedMore) if (buf.len() as u64) < len => {
                return Err(changed(
                    "file ends inside the GGUF key/value section".into(),
                ));
            }
            Ok(Section::NeedMore) if len >= limits.max => {
                return Err(changed(format!(
                    "GGUF key/value section does not fit in {} MiB",
                    limits.max / MIB
                )));
            }
            Ok(Section::NeedMore) => len = (len * 2).min(limits.max),
            Err(why) => return Err(changed(why)),
        }
    }
}

/// Result of scanning a GGUF prefix.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Section {
    /// The key/value section ends at this offset.
    End(usize),
    /// The buffer stops inside the section.
    NeedMore,
}

enum Stop {
    NeedMore,
    Bad(String),
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: u64) -> Result<&'a [u8], Stop> {
        let n = usize::try_from(n).map_err(|_| Stop::Bad("length overflow".into()))?;
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| Stop::Bad("length overflow".into()))?;
        let slice = self.buf.get(self.pos..end).ok_or(Stop::NeedMore)?;
        self.pos = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32, Stop> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, Stop> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
}

fn scalar_size(ty: u32) -> Option<u64> {
    match ty {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

fn skip_value(r: &mut Reader<'_>, ty: u32) -> Result<(), Stop> {
    match ty {
        8 => {
            let n = r.u64()?;
            r.take(n).map(drop)
        }
        9 => {
            let elem = r.u32()?;
            let n = r.u64()?;
            if let Some(size) = scalar_size(elem) {
                let bytes = n
                    .checked_mul(size)
                    .ok_or_else(|| Stop::Bad("array length overflow".into()))?;
                r.take(bytes).map(drop)
            } else {
                (0..n).try_for_each(|_| skip_value(r, elem))
            }
        }
        _ => {
            let size = scalar_size(ty)
                .ok_or_else(|| Stop::Bad(format!("unknown GGUF value type {ty}")))?;
            r.take(size).map(drop)
        }
    }
}

/// Find where the GGUF key/value section of `buf` ends.
pub(crate) fn kv_section_end(buf: &[u8]) -> Result<Section, String> {
    let scan = |buf: &[u8]| -> Result<usize, Stop> {
        let mut r = Reader { buf, pos: 0 };
        if r.take(4)? != b"GGUF" {
            return Err(Stop::Bad("not a GGUF file".into()));
        }
        let version = r.u32()?;
        if !(2..=3).contains(&version) {
            return Err(Stop::Bad(format!("unsupported GGUF version {version}")));
        }
        let _tensors = r.u64()?;
        let kvs = r.u64()?;
        for _ in 0..kvs {
            let key_len = r.u64()?;
            r.take(key_len)?;
            let ty = r.u32()?;
            skip_value(&mut r, ty)?;
        }
        Ok(r.pos)
    };
    match scan(buf) {
        Ok(end) => Ok(Section::End(end)),
        Err(Stop::NeedMore) => Ok(Section::NeedMore),
        Err(Stop::Bad(why)) => Err(why),
    }
}

/// Load `vocab`, fetching it when missing. `None` (with a message on
/// stderr) when it cannot be fetched; panics when a pinned file changed.
pub(crate) fn vocab_model(vocab: Vocab) -> Option<Arc<LlamaModel>> {
    let source = vocab.source();
    let path = match ensure(
        &source,
        &cache_dir(),
        Hosts::PINNED,
        Limits::PINNED,
        &mut curl,
    ) {
        Ok(path) => path,
        Err(VocabError::Unavailable(why)) => {
            eprintln!(
                "SKIPPED: vocabulary unavailable (offline?): {why}; set QMT_LLAMA_VOCAB_DIR to a populated cache"
            );
            return None;
        }
        Err(changed) => panic!("{changed}"),
    };
    let backend = crate::backend::llama_backend().expect("backend");
    let model = LlamaModel::load_from_file(
        &backend,
        &path,
        &LlamaModelParams::default().with_vocab_only(true),
    )
    .unwrap_or_else(|e| {
        panic!(
            "{source}: stored file {} does not load as a vocabulary: {e}",
            path.display()
        )
    });
    Some(Arc::new(model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Removes its directory on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "qmt-vocab-test-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// GGUF with a u32, a string and a string array, then a tensor-info stub.
    /// Returns the bytes and the offset where the key/value section ends.
    fn gguf(extra_string: usize) -> (Vec<u8>, usize) {
        fn string(out: &mut Vec<u8>, s: &[u8]) {
            out.extend((s.len() as u64).to_le_bytes());
            out.extend(s);
        }
        fn key(out: &mut Vec<u8>, name: &str, ty: u32) {
            string(out, name.as_bytes());
            out.extend(ty.to_le_bytes());
        }
        let mut out = b"GGUF".to_vec();
        out.extend(3u32.to_le_bytes());
        out.extend(7u64.to_le_bytes()); // tensors
        out.extend(4u64.to_le_bytes()); // key/values
        key(&mut out, "general.alignment", 4);
        out.extend(32u32.to_le_bytes());
        key(&mut out, "general.name", 8);
        string(&mut out, b"fixture");
        key(&mut out, "tokenizer.ggml.tokens", 9);
        out.extend(8u32.to_le_bytes());
        out.extend(3u64.to_le_bytes());
        for token in ["a", "<|tool_call>", ""] {
            string(&mut out, token.as_bytes());
        }
        key(&mut out, "padding", 8);
        string(&mut out, &vec![b'x'; extra_string]);
        let end = out.len();
        out.extend(b"tensor info follows");
        (out, end)
    }

    /// Lay a fixture out the way Hugging Face serves it.
    fn serve(dir: &Path, hf: &HfFile, bytes: &[u8]) {
        let at = dir.join(hf.repo).join("resolve").join(hf.rev);
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(at.join(hf.file), bytes).unwrap();
    }

    const FIXTURE: HfFile = HfFile {
        repo: "fixture/model-GGUF",
        rev: "0123456789abcdef0123456789abcdef01234567",
        file: "model.gguf",
    };

    fn hosts(base: &str) -> Hosts<'_> {
        Hosts {
            llama_cpp: base,
            hugging_face: base,
        }
    }

    #[test]
    fn kv_section_end_needs_the_whole_section() {
        let (bytes, end) = gguf(100);
        assert_eq!(kv_section_end(&bytes), Ok(Section::End(end)));
        for len in 0..end {
            assert_eq!(
                kv_section_end(&bytes[..len]),
                Ok(Section::NeedMore),
                "{len}"
            );
        }
        assert!(kv_section_end(b"GGML\0\0\0\0").is_err());
    }

    #[test]
    fn prefix_fetch_doubles_until_the_section_fits() {
        let (bytes, end) = gguf(10_000);
        let served = TempDir::new("serve");
        serve(&served.0, &FIXTURE, &bytes);
        let base = format!("file://{}", served.0.display());
        let mut requests = 0;
        let prefix = fetch_prefix(
            &FIXTURE,
            &base,
            Limits {
                initial: 256,
                max: 64 * 1024,
            },
            &mut |url, len| {
                requests += 1;
                curl(url, len)
            },
        )
        .expect("prefix");
        assert!(requests > 1, "expected doubling, got {requests} request(s)");
        assert_eq!(prefix.len(), end);
        assert_eq!(&prefix[8..16], &[0; 8], "tensor count is zeroed");
        assert_eq!(&prefix[..8], &bytes[..8]);
        assert_eq!(&prefix[16..], &bytes[16..end]);
    }

    #[test]
    fn changed_pinned_model_file_fails_loudly() {
        let (bytes, _) = gguf(10_000);
        let served = TempDir::new("serve");
        serve(&served.0, &FIXTURE, &bytes);
        let base = format!("file://{}", served.0.display());
        let cache = TempDir::new("cache");
        let source = Source::HuggingFace(FIXTURE);
        let err = ensure(
            &source,
            &cache.0,
            hosts(&base),
            Limits {
                initial: 1024,
                max: 4096,
            },
            &mut curl,
        )
        .expect_err("section does not fit");
        let VocabError::PinnedFileChanged(message) = err else {
            panic!("expected a loud failure, got {err:?}");
        };
        for part in [FIXTURE.repo, FIXTURE.rev, FIXTURE.file] {
            assert!(message.contains(part), "{part} missing from: {message}");
        }
        assert!(
            std::fs::read_dir(&cache.0).unwrap().next().is_none(),
            "nothing is cached for a changed pin"
        );
    }

    #[test]
    fn offline_run_skips_real_tokenizer_checks() {
        let cache = TempDir::new("cache");
        let source = Vocab::Lfm2.source();
        let err = ensure(
            &source,
            &cache.0,
            hosts("http://127.0.0.1:1"),
            Limits::PINNED,
            &mut curl,
        )
        .expect_err("unreachable");
        let VocabError::Unavailable(message) = err else {
            panic!("expected a skip, got {err:?}");
        };
        assert!(message.contains("LFM2-350M-GGUF"), "{message}");
    }

    #[test]
    fn hugging_face_tokenizers_are_fetched_at_the_pinned_revision() {
        let cache = TempDir::new("cache");
        let source = Vocab::Lfm2.source();
        let requests = std::cell::Cell::new(0u32);
        let mut counting = |url: &str, len: Option<u64>| {
            requests.set(requests.get() + 1);
            assert!(
                url.contains("/resolve/8fdc9d526b7ed346b19257551b05816c7912ecc2/"),
                "{url}"
            );
            assert!(
                len.is_some_and(|n| n <= 64 * MIB),
                "ranged request: {len:?}"
            );
            curl(url, len)
        };
        let path = match ensure(
            &source,
            &cache.0,
            Hosts::PINNED,
            Limits::PINNED,
            &mut counting,
        ) {
            Ok(path) => path,
            Err(VocabError::Unavailable(why)) => {
                eprintln!("SKIPPED: {why}");
                return;
            }
            Err(changed) => panic!("{changed}"),
        };
        assert!(path.metadata().unwrap().len() < 64 * MIB);
        let first = requests.get();
        assert!(first >= 1);
        ensure(
            &source,
            &cache.0,
            Hosts::PINNED,
            Limits::PINNED,
            &mut counting,
        )
        .expect("cached");
        assert_eq!(
            requests.get(),
            first,
            "a cached vocabulary is not fetched again"
        );
    }

    #[test]
    fn real_tokenizers_report_the_expected_vocabulary() {
        for vocab in Vocab::ALL {
            let Some(model) = vocab_model(vocab) else {
                continue;
            };
            assert_eq!(model.n_vocab(), vocab.n_vocab(), "{vocab:?}");
            let single: &[&str] = match vocab {
                Vocab::Qwen35 => &[],
                Vocab::Gemma4 => &["<|tool_call>", "<tool_call|>", "<|\"|>"],
                Vocab::Lfm2 => &["<|tool_call_start|>", "<|tool_call_end|>"],
                Vocab::FunctionGemma => {
                    &["<start_function_call>", "<end_function_call>", "<escape>"]
                }
            };
            for text in single {
                let tokens = model.vocab().tokenize(text.as_bytes(), false, true);
                assert_eq!(tokens.len(), 1, "{vocab:?} {text}: {tokens:?}");
            }
            if vocab == Vocab::Qwen35 {
                let tokens = model.vocab().tokenize(b"<tool_call>", false, true);
                assert!(tokens.len() > 1, "{tokens:?}");
            }
        }
    }
}
