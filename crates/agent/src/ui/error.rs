use anyhow::Error;

/// Include the causal chain so profile config errors surface actionable paths
/// such as missing prompt files in the dashboard error text.
pub(crate) fn format_error_chain(err: &Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in err.chain() {
        let message = cause.to_string();
        if parts.last() != Some(&message) {
            parts.push(message);
        }
    }
    parts.join(": ")
}

pub(crate) fn format_prefixed_error_chain(prefix: &str, err: &Error) -> String {
    format!("{prefix}: {}", format_error_chain(err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn format_error_chain_includes_contexts_and_root_cause() {
        let err = anyhow!("root cause")
            .context("middle context")
            .context("top context");

        let message = format_error_chain(&err);

        assert_eq!(message, "top context: middle context: root cause");
    }
}
