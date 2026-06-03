// schemars helper functions
// ============================================================================

/// Schema for the `system` field: accepts a plain string or an array of
/// inline strings / `{ file = "..." }` objects.
pub fn schema_for_system_parts(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
    serde_json::from_value(serde_json::json!({
        "oneOf": [
            {
                "type": "string",
                "description": "A single inline system prompt string"
            },
            {
                "type": "array",
                "description": "An array of inline strings and/or file references",
                "items": {
                    "oneOf": [
                        {
                            "type": "string",
                            "description": "Inline system prompt text"
                        },
                        {
                            "type": "object",
                            "description": "A reference to a file whose contents will be used as a system prompt part",
                            "required": ["file"],
                            "properties": {
                                "file": { "type": "string" }
                            },
                            "additionalProperties": false
                        }
                    ]
                }
            }
        ]
    }))
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
