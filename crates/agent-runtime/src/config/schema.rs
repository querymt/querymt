// schemars helper functions
// ============================================================================

/// Schema for the `system` field: accepts a plain string or an array of
/// inline strings / `{ file = "..." }` objects.
pub fn schema_for_system_parts(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let cache = generator.subschema_for::<querymt::chat::CacheHint>();
    let part = serde_json::json!({"oneOf": [
        {"type": "string"},
        {"type": "object", "required": ["text"], "properties": {
            "text": {"type": "string"}, "cache": cache
        }, "additionalProperties": false},
        {"type": "object", "required": ["file"], "properties": {
            "file": {"type": "string"}, "cache": cache
        }, "additionalProperties": false}
    ]});
    serde_json::from_value(serde_json::json!({"oneOf": [
        {"type": "string"}, {"type": "array", "items": part}
    ]}))
    .expect("system schema is valid JSON Schema")
}

/// Schema for an open-ended JSON object (used for `MiddlewareEntry.config`).
pub fn schema_for_value(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "description": "Middleware-specific configuration fields. Valid keys depend on the `type`.",
        "additionalProperties": true
    }))
    .expect("value schema is valid JSON Schema")
}
