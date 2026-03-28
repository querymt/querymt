use crate::session::store::LLMConfig;
use time::OffsetDateTime;

pub(super) fn parse_llm_params(params: &str) -> Result<Option<serde_json::Value>, rusqlite::Error> {
    if params.trim().is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_str(params).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    if value.as_object().is_none_or(|obj| obj.is_empty()) {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

pub(super) fn parse_llm_config_row(row: &rusqlite::Row<'_>) -> Result<LLMConfig, rusqlite::Error> {
    let params_str: String = row.get(4)?;
    Ok(LLMConfig {
        id: row.get(0)?,
        name: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
        params: parse_llm_params(&params_str)?,
        created_at: row.get::<_, Option<String>>(5)?.and_then(|s| {
            OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok()
        }),
        updated_at: row.get::<_, Option<String>>(6)?.and_then(|s| {
            OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok()
        }),
        provider_node_id: None,
    })
}
