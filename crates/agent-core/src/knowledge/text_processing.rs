//! Multilingual text processing for the knowledge layer.
//!
//! Provides language-aware stopword filtering, stemming, and keyword
//! extraction. Used by the SQLite knowledge store for query building
//! and entity/topic normalization.
//!
//! ## Crates Used
//!
//! - [`whatlang`] — lightweight language detection (no external deps)
//! - [`stop_words`] — curated stopword lists for 50+ languages (ISO/NLTK)
//! - [`rust_stemmers`] — Snowball stemming algorithms for 19 languages

use rust_stemmers::{Algorithm, Stemmer};
use std::borrow::Cow;
use std::collections::HashSet;

// ─── Language Detection ──────────────────────────────────────────────────────

/// Detected language with mapping to stopword list and stemmer algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectedLanguage {
    /// ISO 639-1 code for stop-words crate lookup.
    pub stopword_code: &'static str,
    /// Snowball stemmer algorithm, if available for this language.
    pub stemmer_algorithm: Option<Algorithm>,
}

/// Detect the language of the given text and return mappings for NLP tools.
///
/// Falls back to English if the text is too short or ambiguous for detection.
/// This is intentional: short queries (2-3 words) rarely contain enough signal
/// for reliable detection, and English is the most common case.
pub fn detect_language(text: &str) -> DetectedLanguage {
    // whatlang needs reasonable text length for accuracy.
    // For very short inputs, fall back to English.
    if text.split_whitespace().count() < 3 {
        return ENGLISH;
    }

    match whatlang::detect_lang(text) {
        Some(lang) => whatlang_to_nlp(lang),
        None => ENGLISH,
    }
}

const ENGLISH: DetectedLanguage = DetectedLanguage {
    stopword_code: "en",
    stemmer_algorithm: Some(Algorithm::English),
};

/// Map whatlang's Lang enum to our stopword code + stemmer algorithm.
///
/// Only languages that have BOTH a stop-words list AND a Snowball stemmer
/// get full support. Languages with only stopwords get stopword filtering
/// but no stemming. Languages with neither fall back to English.
fn whatlang_to_nlp(lang: whatlang::Lang) -> DetectedLanguage {
    use whatlang::Lang;
    match lang {
        Lang::Eng => DetectedLanguage {
            stopword_code: "en",
            stemmer_algorithm: Some(Algorithm::English),
        },
        Lang::Deu => DetectedLanguage {
            stopword_code: "de",
            stemmer_algorithm: Some(Algorithm::German),
        },
        Lang::Fra => DetectedLanguage {
            stopword_code: "fr",
            stemmer_algorithm: Some(Algorithm::French),
        },
        Lang::Spa => DetectedLanguage {
            stopword_code: "es",
            stemmer_algorithm: Some(Algorithm::Spanish),
        },
        Lang::Por => DetectedLanguage {
            stopword_code: "pt",
            stemmer_algorithm: Some(Algorithm::Portuguese),
        },
        Lang::Ita => DetectedLanguage {
            stopword_code: "it",
            stemmer_algorithm: Some(Algorithm::Italian),
        },
        Lang::Nld => DetectedLanguage {
            stopword_code: "nl",
            stemmer_algorithm: Some(Algorithm::Dutch),
        },
        Lang::Rus => DetectedLanguage {
            stopword_code: "ru",
            stemmer_algorithm: Some(Algorithm::Russian),
        },
        Lang::Dan => DetectedLanguage {
            stopword_code: "da",
            stemmer_algorithm: Some(Algorithm::Danish),
        },
        Lang::Swe => DetectedLanguage {
            stopword_code: "sv",
            stemmer_algorithm: Some(Algorithm::Swedish),
        },
        Lang::Fin => DetectedLanguage {
            stopword_code: "fi",
            stemmer_algorithm: Some(Algorithm::Finnish),
        },
        Lang::Hun => DetectedLanguage {
            stopword_code: "hu",
            stemmer_algorithm: Some(Algorithm::Hungarian),
        },
        Lang::Tur => DetectedLanguage {
            stopword_code: "tr",
            stemmer_algorithm: Some(Algorithm::Turkish),
        },
        Lang::Ron => DetectedLanguage {
            stopword_code: "ro",
            stemmer_algorithm: Some(Algorithm::Romanian),
        },
        Lang::Ell => DetectedLanguage {
            stopword_code: "el",
            stemmer_algorithm: Some(Algorithm::Greek),
        },
        Lang::Ara => DetectedLanguage {
            stopword_code: "ar",
            stemmer_algorithm: Some(Algorithm::Arabic),
        },
        Lang::Tam => DetectedLanguage {
            stopword_code: "ta",
            stemmer_algorithm: Some(Algorithm::Tamil),
        },
        Lang::Hye => DetectedLanguage {
            stopword_code: "hy",
            stemmer_algorithm: None,
        },
        Lang::Nob => DetectedLanguage {
            stopword_code: "no",
            stemmer_algorithm: Some(Algorithm::Norwegian),
        },
        // Languages with stopwords but no Snowball stemmer
        Lang::Pol => DetectedLanguage {
            stopword_code: "pl",
            stemmer_algorithm: None,
        },
        Lang::Ukr => DetectedLanguage {
            stopword_code: "uk",
            stemmer_algorithm: None,
        },
        Lang::Ces => DetectedLanguage {
            stopword_code: "cs",
            stemmer_algorithm: None,
        },
        Lang::Bul => DetectedLanguage {
            stopword_code: "bg",
            stemmer_algorithm: None,
        },
        Lang::Hrv => DetectedLanguage {
            stopword_code: "hr",
            stemmer_algorithm: None,
        },
        Lang::Slk => DetectedLanguage {
            stopword_code: "sk",
            stemmer_algorithm: None,
        },
        Lang::Slv => DetectedLanguage {
            stopword_code: "sl",
            stemmer_algorithm: None,
        },
        Lang::Lit => DetectedLanguage {
            stopword_code: "lt",
            stemmer_algorithm: None,
        },
        Lang::Lav => DetectedLanguage {
            stopword_code: "lv",
            stemmer_algorithm: None,
        },
        Lang::Est => DetectedLanguage {
            stopword_code: "et",
            stemmer_algorithm: None,
        },
        Lang::Ind => DetectedLanguage {
            stopword_code: "id",
            stemmer_algorithm: None,
        },
        Lang::Hin => DetectedLanguage {
            stopword_code: "hi",
            stemmer_algorithm: None,
        },
        Lang::Kor => DetectedLanguage {
            stopword_code: "ko",
            stemmer_algorithm: None,
        },
        Lang::Jpn => DetectedLanguage {
            stopword_code: "ja",
            stemmer_algorithm: None,
        },
        Lang::Cmn => DetectedLanguage {
            stopword_code: "zh",
            stemmer_algorithm: None,
        },
        Lang::Vie => DetectedLanguage {
            stopword_code: "vi",
            stemmer_algorithm: None,
        },
        Lang::Tha => DetectedLanguage {
            stopword_code: "th",
            stemmer_algorithm: None,
        },
        Lang::Afr => DetectedLanguage {
            stopword_code: "af",
            stemmer_algorithm: None,
        },
        Lang::Cat => DetectedLanguage {
            stopword_code: "ca",
            stemmer_algorithm: None,
        },
        Lang::Heb => DetectedLanguage {
            stopword_code: "he",
            stemmer_algorithm: None,
        },
        Lang::Pes => DetectedLanguage {
            stopword_code: "fa",
            stemmer_algorithm: None,
        },
        // Languages without stopword support — fall back to English
        _ => ENGLISH,
    }
}

// ─── Stopword Filtering ─────────────────────────────────────────────────────

/// Get the stopword set for a language code.
///
/// Returns an empty set if the language code is not recognized by the
/// `stop-words` crate (should not happen given our mapping, but defensive).
fn get_stopwords(code: &str) -> HashSet<&'static str> {
    // stop_words::get panics on unknown codes, so we catch it
    std::panic::catch_unwind(|| stop_words::get(code))
        .map(|words| words.iter().copied().collect())
        .unwrap_or_default()
}

// ─── Keyword Extraction ─────────────────────────────────────────────────────

/// Extract keywords from a question string with language-aware processing.
///
/// Steps:
/// 1. Detect language of the input text
/// 2. Tokenize on non-alphanumeric boundaries (preserving `_` and `-`)
/// 3. Lowercase all tokens
/// 4. Remove stopwords for the detected language
/// 5. Filter tokens shorter than 2 characters
///
/// Returns `(keywords, detected_language)` so callers can use the detected
/// language for stemming if needed.
pub fn extract_keywords(question: &str) -> (Vec<String>, DetectedLanguage) {
    let lang = detect_language(question);
    let stopwords = get_stopwords(lang.stopword_code);

    let keywords: Vec<String> = question
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|w| w.len() >= 2 && !stopwords.contains(w))
        .map(String::from)
        .collect();

    (keywords, lang)
}

/// Stem a single word using the Snowball stemmer for the given language.
///
/// Returns the original word unchanged if no stemmer is available for
/// the language (e.g. Chinese, Japanese, Korean).
pub fn stem_word(word: &str, lang: &DetectedLanguage) -> String {
    match lang.stemmer_algorithm {
        Some(algo) => {
            let stemmer = Stemmer::create(algo);
            let stemmed: Cow<'_, str> = stemmer.stem(word);
            stemmed.into_owned()
        }
        None => word.to_string(),
    }
}

/// Stem a list of keywords.
pub fn stem_keywords(keywords: &[String], lang: &DetectedLanguage) -> Vec<String> {
    match lang.stemmer_algorithm {
        Some(algo) => {
            let stemmer = Stemmer::create(algo);
            keywords
                .iter()
                .map(|kw| {
                    let stemmed: Cow<'_, str> = stemmer.stem(kw);
                    stemmed.into_owned()
                })
                .collect()
        }
        None => keywords.to_vec(),
    }
}

/// Normalize a single term for storage/matching: lowercase + stem.
///
/// Used at ingest time to normalize entities and topics so that
/// query-time matching is consistent.
pub fn normalize_term(term: &str) -> String {
    let lower = term.to_lowercase();
    // For normalization at ingest time, use English stemmer as default
    // since entity/topic names are typically in the same language as the
    // content. We could detect per-term but that's unreliable for single
    // words — English Porter stemming is a reasonable universal baseline
    // that doesn't mangle non-English terms badly.
    let stemmer = Stemmer::create(Algorithm::English);
    let stemmed: Cow<'_, str> = stemmer.stem(&lower);
    stemmed.into_owned()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_stopwords_filtered() {
        let (keywords, lang) = extract_keywords("the quick brown fox jumps over the lazy dog");
        assert_eq!(lang.stopword_code, "en");
        // "the", "over" should be filtered
        assert!(!keywords.contains(&"the".to_string()));
        assert!(!keywords.contains(&"over".to_string()));
        assert!(keywords.contains(&"quick".to_string()));
        assert!(keywords.contains(&"brown".to_string()));
        assert!(keywords.contains(&"fox".to_string()));
    }

    #[test]
    fn german_stopwords_filtered() {
        let (keywords, lang) =
            extract_keywords("Der schnelle braune Fuchs springt über den faulen Hund");
        assert_eq!(lang.stopword_code, "de");
        assert_eq!(lang.stemmer_algorithm, Some(Algorithm::German));
        // "der", "den" should be filtered as German stopwords
        assert!(!keywords.contains(&"der".to_string()));
        assert!(!keywords.contains(&"den".to_string()));
    }

    #[test]
    fn french_stopwords_filtered() {
        let (keywords, lang) =
            extract_keywords("Le renard brun rapide saute par-dessus le chien paresseux");
        assert_eq!(lang.stopword_code, "fr");
        assert_eq!(lang.stemmer_algorithm, Some(Algorithm::French));
        // "le" should be filtered
        assert!(!keywords.contains(&"le".to_string()));
    }

    #[test]
    fn short_text_defaults_to_english() {
        let (_, lang) = extract_keywords("hello world");
        assert_eq!(lang.stopword_code, "en");
    }

    #[test]
    fn english_stemming() {
        let lang = DetectedLanguage {
            stopword_code: "en",
            stemmer_algorithm: Some(Algorithm::English),
        };
        assert_eq!(stem_word("configuring", &lang), "configur");
        assert_eq!(stem_word("configuration", &lang), "configur");
        assert_eq!(stem_word("configured", &lang), "configur");
    }

    #[test]
    fn german_stemming() {
        let lang = DetectedLanguage {
            stopword_code: "de",
            stemmer_algorithm: Some(Algorithm::German),
        };
        // German stemmer should reduce inflected forms
        let stemmed = stem_word("springt", &lang);
        assert!(!stemmed.is_empty());
    }

    #[test]
    fn no_stemmer_returns_original() {
        let lang = DetectedLanguage {
            stopword_code: "zh",
            stemmer_algorithm: None,
        };
        assert_eq!(stem_word("hello", &lang), "hello");
    }

    #[test]
    fn normalize_term_lowercases_and_stems() {
        let result = normalize_term("Configuration");
        assert_eq!(result, "configur");
    }

    #[test]
    fn normalize_term_preserves_short_words() {
        // Short words still get stemmed if possible
        let result = normalize_term("go");
        assert!(!result.is_empty());
    }

    #[test]
    fn stem_keywords_batch() {
        let lang = DetectedLanguage {
            stopword_code: "en",
            stemmer_algorithm: Some(Algorithm::English),
        };
        let keywords = vec!["running".to_string(), "jumped".to_string()];
        let stemmed = stem_keywords(&keywords, &lang);
        assert_eq!(stemmed[0], "run");
        assert_eq!(stemmed[1], "jump");
    }

    #[test]
    fn extract_keywords_preserves_underscores_and_hyphens() {
        let (keywords, _) = extract_keywords("the dark-mode user_preference is important");
        assert!(keywords.contains(&"dark-mode".to_string()));
        assert!(keywords.contains(&"user_preference".to_string()));
    }

    #[test]
    fn extract_keywords_empty_input() {
        let (keywords, _) = extract_keywords("");
        assert!(keywords.is_empty());
    }

    #[test]
    fn extract_keywords_only_stopwords() {
        let (keywords, _) = extract_keywords("the a an is are was were");
        assert!(keywords.is_empty());
    }
}
