use super::*;

/// Session-independent speech settings (`[speech]`).
///
/// Absent or empty configuration disables the speech ACP methods. Clients
/// select a profile by `id`; provider credentials stay in this config.
///
/// ```toml
/// [speech]
/// default_transcription = "local-stt"
/// default_synthesis = "local-tts"
///
/// [[speech.profiles]]
/// id = "local-stt"
/// provider = "izwi"
/// model = "Qwen3-ASR-0.6B"
///
/// [[speech.profiles]]
/// id = "local-tts"
/// provider = "izwi"
/// model = "Qwen3-TTS-12Hz-0.6B-CustomVoice-4bit"
/// operation = "synthesis"
/// voice = "vivian"
///
/// [[speech.profiles]]
/// id = "local-tts-design"
/// provider = "izwi"
/// model = "Qwen3-TTS-12Hz-1.7B-VoiceDesign-4bit"
/// operation = "synthesis"
/// voice_description = "A warm female voice, mid-30s, slight British accent"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SpeechConfig {
    /// Profile `id` used when `querymt/speech/transcribe` omits `profile`.
    pub default_transcription: Option<String>,

    /// Profile `id` used when `querymt/speech/synthesize` omits `profile`.
    pub default_synthesis: Option<String>,

    /// Named speech profiles. Each `id` is unique and non-empty.
    pub profiles: Vec<SpeechProfile>,
}

/// Whether a speech profile is used for STT or TTS.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpeechOperation {
    #[default]
    Transcription,
    Synthesis,
}

impl SpeechOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transcription => "transcription",
            Self::Synthesis => "synthesis",
        }
    }
}

/// One named speech backend.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpeechProfile {
    /// Client-facing profile identifier.
    pub id: String,
    /// Provider name from `providers.toml` (e.g. `"izwi"`, `"openai"`).
    pub provider: String,
    /// Model identifier passed to the provider.
    pub model: String,
    /// Defaults to transcription when omitted.
    #[serde(default)]
    pub operation: SpeechOperation,
    /// Optional API key. Supports `${VAR}` interpolation at load time.
    pub api_key: Option<String>,
    /// Optional API base URL override.
    pub base_url: Option<String>,
    /// Default preset speaker for CustomVoice / Kokoro / similar models.
    ///
    /// Used when `querymt/speech/synthesize` omits `voice`. Request `voice`
    /// still selects a preset speaker instead. Takes precedence over
    /// `voice_description` when both are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Natural-language voice description for VoiceDesign models.
    ///
    /// Used when synthesize omits `voice` and this profile has no `voice`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_description: Option<String>,
}

impl SpeechConfig {
    /// True when at least one profile is configured.
    pub fn is_configured(&self) -> bool {
        !self.profiles.is_empty()
    }

    /// Unique non-empty profile ids; defaults must name a matching profile.
    ///
    /// # Errors
    ///
    /// Returns a description of the first validation failure.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = HashSet::with_capacity(self.profiles.len());
        for profile in &self.profiles {
            if profile.id.is_empty() {
                return Err("speech profile id must be non-empty".into());
            }
            if !seen.insert(profile.id.as_str()) {
                return Err(format!("duplicate speech profile id '{}'", profile.id));
            }
            validate_synthesis_option(profile, "voice", profile.voice.as_deref())?;
            validate_synthesis_option(
                profile,
                "voice_description",
                profile.voice_description.as_deref(),
            )?;
        }
        self.validate_default(
            "speech.default_transcription",
            self.default_transcription.as_deref(),
            SpeechOperation::Transcription,
        )?;
        self.validate_default(
            "speech.default_synthesis",
            self.default_synthesis.as_deref(),
            SpeechOperation::Synthesis,
        )?;
        Ok(())
    }

    fn validate_default(
        &self,
        field: &str,
        default: Option<&str>,
        operation: SpeechOperation,
    ) -> Result<(), String> {
        let Some(default) = default else {
            return Ok(());
        };
        let Some(profile) = self.profiles.iter().find(|profile| profile.id == default) else {
            return Err(format!("{field} '{default}' does not match any profile"));
        };
        if profile.operation != operation {
            return Err(format!(
                "{field} '{default}' is not a {} profile",
                operation.as_str()
            ));
        }
        Ok(())
    }
}

fn validate_synthesis_option(
    profile: &SpeechProfile,
    field: &str,
    value: Option<&str>,
) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.trim().is_empty() {
        return Err(format!(
            "speech profile '{}' {field} must be non-empty",
            profile.id
        ));
    }
    if profile.operation != SpeechOperation::Synthesis {
        return Err(format!(
            "speech profile '{}' {field} is only valid for synthesis",
            profile.id
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> SpeechConfig {
        toml::from_str(toml).expect("speech config should parse")
    }

    #[test]
    fn empty_config_is_unconfigured_and_valid() {
        let config = SpeechConfig::default();
        assert!(!config.is_configured());
        config.validate().unwrap();
    }

    #[test]
    fn parses_profiles_and_default() {
        let config = parse(
            r#"
default_transcription = "local-stt"

[[profiles]]
id = "local-stt"
provider = "izwi"
model = "Qwen3-ASR-0.6B"

[[profiles]]
id = "cloud-stt"
provider = "openai"
model = "gpt-4o-mini-transcribe"
api_key = "sk-test"
base_url = "https://example.invalid"
"#,
        );
        assert!(config.is_configured());
        config.validate().unwrap();
        assert_eq!(config.default_transcription.as_deref(), Some("local-stt"));
        assert_eq!(config.default_synthesis, None);
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.profiles[0].operation, SpeechOperation::Transcription);
        assert_eq!(config.profiles[1].api_key.as_deref(), Some("sk-test"));
        assert_eq!(
            config.profiles[1].base_url.as_deref(),
            Some("https://example.invalid")
        );
    }

    #[test]
    fn parses_synthesis_profile_and_default() {
        let config = parse(
            r#"
default_transcription = "local-stt"
default_synthesis = "local-tts"

[[profiles]]
id = "local-stt"
provider = "izwi"
model = "Qwen3-ASR-0.6B"

[[profiles]]
id = "local-tts"
provider = "izwi"
model = "Qwen3-TTS-12Hz-0.6B-CustomVoice-4bit"
operation = "synthesis"
"#,
        );
        assert!(config.is_configured());
        config.validate().unwrap();
        assert_eq!(config.default_synthesis.as_deref(), Some("local-tts"));
        assert_eq!(config.profiles[1].operation, SpeechOperation::Synthesis);
        assert_eq!(config.profiles[1].voice, None);
        assert_eq!(config.profiles[1].voice_description, None);
    }

    #[test]
    fn parses_synthesis_voice() {
        let config = parse(
            r#"
default_synthesis = "local-tts"

[[profiles]]
id = "local-tts"
provider = "izwi"
model = "Qwen3-TTS-12Hz-0.6B-CustomVoice-4bit"
operation = "synthesis"
voice = "vivian"
"#,
        );
        config.validate().unwrap();
        assert_eq!(config.profiles[0].voice.as_deref(), Some("vivian"));
    }

    #[test]
    fn parses_synthesis_voice_description() {
        let config = parse(
            r#"
default_synthesis = "local-tts"

[[profiles]]
id = "local-tts"
provider = "izwi"
model = "Qwen3-TTS-12Hz-1.7B-VoiceDesign-4bit"
operation = "synthesis"
voice_description = "A warm female voice, mid-30s, slight British accent"
"#,
        );
        config.validate().unwrap();
        assert_eq!(
            config.profiles[0].voice_description.as_deref(),
            Some("A warm female voice, mid-30s, slight British accent")
        );
    }

    #[test]
    fn rejects_voice_description_on_transcription_profile() {
        let config = parse(
            r#"
[[profiles]]
id = "local-stt"
provider = "izwi"
model = "Qwen3-ASR-0.6B"
voice_description = "A warm female voice"
"#,
        );
        let err = config.validate().unwrap_err();
        assert!(err.contains("only valid for synthesis"), "{err}");
    }

    #[test]
    fn rejects_empty_voice_description() {
        let config = parse(
            r#"
[[profiles]]
id = "local-tts"
provider = "izwi"
model = "Qwen3-TTS-12Hz-1.7B-VoiceDesign-4bit"
operation = "synthesis"
voice_description = "   "
"#,
        );
        let err = config.validate().unwrap_err();
        assert!(err.contains("must be non-empty"), "{err}");
    }

    #[test]
    fn rejects_voice_on_transcription_profile() {
        let config = parse(
            r#"
[[profiles]]
id = "local-stt"
provider = "izwi"
model = "Qwen3-ASR-0.6B"
voice = "vivian"
"#,
        );
        let err = config.validate().unwrap_err();
        assert!(err.contains("only valid for synthesis"), "{err}");
    }

    #[test]
    fn rejects_empty_voice() {
        let config = parse(
            r#"
[[profiles]]
id = "local-tts"
provider = "izwi"
model = "Qwen3-TTS-12Hz-0.6B-CustomVoice-4bit"
operation = "synthesis"
voice = "   "
"#,
        );
        let err = config.validate().unwrap_err();
        assert!(err.contains("must be non-empty"), "{err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = toml::from_str::<SpeechConfig>("voice = \"alloy\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn rejects_unknown_profile_fields() {
        let err = toml::from_str::<SpeechConfig>(
            r#"
[[profiles]]
id = "local-stt"
provider = "izwi"
model = "Qwen3-ASR-0.6B"
foo = 1
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn rejects_empty_profile_id() {
        let config = parse(
            r#"
[[profiles]]
id = ""
provider = "izwi"
model = "Qwen3-ASR-0.6B"
"#,
        );
        let err = config.validate().unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
    }

    #[test]
    fn rejects_duplicate_profile_ids() {
        let config = parse(
            r#"
[[profiles]]
id = "stt"
provider = "izwi"
model = "a"

[[profiles]]
id = "stt"
provider = "openai"
model = "b"
"#,
        );
        let err = config.validate().unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn rejects_missing_default_profile() {
        let config = parse(
            r#"
default_transcription = "missing"

[[profiles]]
id = "local-stt"
provider = "izwi"
model = "Qwen3-ASR-0.6B"
"#,
        );
        let err = config.validate().unwrap_err();
        assert!(err.contains("does not match any profile"), "{err}");
    }

    #[test]
    fn default_without_profiles_is_invalid() {
        let config = parse(r#"default_transcription = "local-stt""#);
        let err = config.validate().unwrap_err();
        assert!(err.contains("does not match any profile"), "{err}");
    }

    #[test]
    fn rejects_transcription_default_pointing_at_synthesis_profile() {
        let config = parse(
            r#"
default_transcription = "local-tts"

[[profiles]]
id = "local-tts"
provider = "izwi"
model = "Qwen3-TTS-12Hz-0.6B-CustomVoice-4bit"
operation = "synthesis"
"#,
        );
        let err = config.validate().unwrap_err();
        assert!(err.contains("is not a transcription profile"), "{err}");
    }

    #[test]
    fn rejects_missing_synthesis_default_profile() {
        let config = parse(
            r#"
default_synthesis = "missing"

[[profiles]]
id = "local-stt"
provider = "izwi"
model = "Qwen3-ASR-0.6B"
"#,
        );
        let err = config.validate().unwrap_err();
        assert!(err.contains("does not match any profile"), "{err}");
    }
}
