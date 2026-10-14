//! Responses cache intent resolution, shared with the Codex serializer.
use querymt::{chat::CacheHint, error::LLMError};
use serde::Serialize;
use serde_json::{Map, Value};

/// Only these input content blocks accept explicit boundaries in Responses.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CacheableContent<'a> {
    InputText {
        text: std::borrow::Cow<'a, str>,
    },
    InputImage {
        image_url: std::borrow::Cow<'a, str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<std::borrow::Cow<'a, str>>,
    },
    InputFile {
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<std::borrow::Cow<'a, str>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_data: Option<std::borrow::Cow<'a, str>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_url: Option<std::borrow::Cow<'a, str>>,
    },
}

/// Typed decoration of a supported content block; adds no extra wire nesting.
#[derive(Debug, Serialize)]
pub struct CachedContent<T> {
    #[serde(flatten)]
    pub content: T,
    pub prompt_cache_breakpoint: super::api::PromptCacheBreakpoint,
}
impl<T> CachedContent<T> {
    pub fn new(content: T) -> Self {
        Self {
            content,
            prompt_cache_breakpoint: super::api::PromptCacheBreakpoint::explicit(),
        }
    }
}

/// Only successfully placed hints participate in request-wide TTL resolution.
#[derive(Default)]
pub struct CacheHints {
    applied: usize,
    ttl_seconds: Option<u64>,
}
impl CacheHints {
    pub fn applied(&mut self, hint: &CacheHint) -> Result<(), LLMError> {
        let CacheHint::Ephemeral { ttl_seconds } = hint;
        log::debug!("Responses CacheHint requested: ttl_seconds={ttl_seconds:?}");
        if let Some(seconds) = ttl_seconds {
            if *seconds != 1800 {
                return Err(LLMError::InvalidRequest(format!(
                    "Responses cache hints support only ttl_seconds=1800 (30m), got {seconds}"
                )));
            }
            if self
                .ttl_seconds
                .is_some_and(|previous| previous != *seconds)
            {
                return Err(LLMError::InvalidRequest(
                    "conflicting Responses cache hint TTLs".into(),
                ));
            }
            self.ttl_seconds = Some(*seconds);
        }
        self.applied += 1;
        Ok(())
    }

    pub fn finish(self, extra: &mut Map<String, Value>) -> Result<(), LLMError> {
        let options = extra.get("prompt_cache_options");
        if let Some(value) = options {
            if !value.is_object() {
                return Err(LLMError::InvalidRequest(
                    "prompt_cache_options must be an object".into(),
                ));
            }
            if let Some(ttl) = value.get("ttl")
                && ttl.as_str() != Some("30m")
            {
                return Err(LLMError::InvalidRequest("Responses cache TTL must be '30m'; it applies to every breakpoint in the request".into()));
            }
        }
        if self.ttl_seconds.is_some() {
            extra
                .entry("prompt_cache_options")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .expect("validated options")
                .insert("ttl".into(), Value::String("30m".into()));
        }
        if self.applied == 0
            && extra
                .get("prompt_cache_options")
                .and_then(|v| v.get("mode"))
                .and_then(Value::as_str)
                == Some("explicit")
        {
            log::debug!("explicit-only Responses caching has no applied CacheHint boundaries");
        }
        log::debug!(
            "Responses cache hints: applied={} ttl_seconds={:?}",
            self.applied,
            self.ttl_seconds
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ttl_is_request_scoped_and_mode_is_preserved() {
        let mut hints = CacheHints::default();
        hints
            .applied(&CacheHint::Ephemeral { ttl_seconds: None })
            .unwrap();
        hints
            .applied(&CacheHint::Ephemeral {
                ttl_seconds: Some(1800),
            })
            .unwrap();
        let mut extra = serde_json::json!({"prompt_cache_options": {"mode": "explicit", "comparison_response_id": "resp_test"}}).as_object().unwrap().clone();
        hints.finish(&mut extra).unwrap();
        assert_eq!(extra["prompt_cache_options"]["mode"], "explicit");
        assert_eq!(extra["prompt_cache_options"]["ttl"], "30m");
        assert_eq!(
            extra["prompt_cache_options"]["comparison_response_id"],
            "resp_test"
        );
    }
    #[test]
    fn defaults_emit_nothing_and_invalid_ttls_fail() {
        let mut hints = CacheHints::default();
        hints
            .applied(&CacheHint::Ephemeral { ttl_seconds: None })
            .unwrap();
        let mut extra = Map::new();
        hints.finish(&mut extra).unwrap();
        assert!(extra.is_empty());
        assert!(
            CacheHints::default()
                .applied(&CacheHint::Ephemeral {
                    ttl_seconds: Some(300)
                })
                .is_err()
        );
        let mut extra = serde_json::json!({"prompt_cache_options": {"ttl": "1h"}})
            .as_object()
            .unwrap()
            .clone();
        assert!(CacheHints::default().finish(&mut extra).is_err());
    }
}
