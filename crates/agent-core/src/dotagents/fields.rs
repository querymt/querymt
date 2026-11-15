//! Field helpers shared by the protocol document parsers.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Split parsed frontmatter into recognized protocol fields and extension
/// fields, keyed case-insensitively for the recognized set.
///
/// The first matching key wins for a recognized field, so a document that
/// spells the same concept twice keeps deterministic behavior.
///
/// Recognized fields are re-keyed to the canonical spelling supplied in
/// `known`, because the [`super::frontmatter`] field accessors look keys up
/// verbatim. Protocol documents use camelCase (`intervalMinutes`,
/// `runOnStartup`), so a case-insensitive match must still be stored under the
/// accessor's expected spelling.
pub fn partition_fields(
    fields: &BTreeMap<String, Value>,
    known: &[&str],
) -> (BTreeMap<String, Value>, BTreeMap<String, Value>) {
    let mut recognized: BTreeMap<String, Value> = BTreeMap::new();
    let mut extensions: BTreeMap<String, Value> = BTreeMap::new();

    for (key, value) in fields {
        match known
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(key))
        {
            Some(canonical) => {
                recognized
                    .entry((*canonical).to_string())
                    .or_insert_with(|| value.clone());
            }
            None => {
                extensions.insert(key.clone(), value.clone());
            }
        }
    }
    (recognized, extensions)
}

/// A content fingerprint of the raw source text.
///
/// Fingerprints are stable across runs and platforms so they can be used as
/// reconciliation and approval keys. SHA-256 keeps persisted fingerprints
/// deterministic across processes and platforms.
pub fn fingerprint(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
