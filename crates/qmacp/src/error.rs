use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Ok,
    Usage,
    Connection,
    Rpc,
    Timeout,
}

impl ExitCode {
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::Usage => 1,
            Self::Connection => 2,
            Self::Rpc => 3,
            Self::Timeout => 4,
        }
    }
}

#[derive(Debug)]
pub enum CliError {
    Usage(String),
    Connection(String),
    Rpc(String),
    Timeout(String),
    /// An operation failed after acquiring identifiers needed for safe recovery.
    Recovery {
        source: Box<CliError>,
        context: serde_json::Value,
    },
}

impl CliError {
    /// Attach recovery data without changing the error's message or exit code.
    pub fn with_recovery(self, mut context: serde_json::Value) -> Self {
        match self {
            Self::Recovery {
                source,
                context: known,
            } => {
                if let (Some(context), Some(known)) = (context.as_object_mut(), known.as_object()) {
                    context.extend(known.clone());
                }
                Self::Recovery { source, context }
            }
            source => Self::Recovery {
                source: Box::new(source),
                context,
            },
        }
    }

    /// Identifiers and delivery state available after a partial operation.
    pub fn recovery(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Recovery { context, .. } => Some(context),
            _ => None,
        }
    }

    pub fn rpc(error: impl ToString) -> Self {
        Self::Rpc(error.to_string())
    }

    /// Classify notification write failures without changing non-timeout RPC errors.
    pub fn from_notification(error: anyhow::Error) -> Self {
        if error.is::<tokio::time::error::Elapsed>() {
            Self::Timeout(error.to_string())
        } else {
            Self::rpc(error)
        }
    }

    pub fn from_request(error: anyhow::Error) -> Self {
        let message = format!("{error:#}");
        if error.is::<crate::client::RpcResponseError>() {
            Self::Rpc(message)
        } else if message.contains("timed out") {
            Self::Timeout(message)
        } else if message.contains("connect ")
            || message.contains("Connection refused")
            || message.contains("Network is unreachable")
            || message.contains("Name or service not known")
            || message.contains("WebSocket")
        {
            Self::Connection(message)
        } else {
            Self::Rpc(message)
        }
    }

    pub fn connection_with_cause(endpoint: &str, error: impl std::fmt::Display) -> Self {
        let message = error.to_string();
        let hint = if message.contains("Connection refused") {
            "; is qmtcode --acp-ws running? loopback is 127.0.0.1, not 172.0.0.1"
        } else if message.contains("timed out") {
            "; connection timed out"
        } else {
            ""
        };
        Self::Connection(format!("connect {endpoint}: {message}{hint}"))
    }

    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Usage(_) => ExitCode::Usage,
            Self::Connection(_) => ExitCode::Connection,
            Self::Rpc(_) => ExitCode::Rpc,
            Self::Timeout(_) => ExitCode::Timeout,
            Self::Recovery { source, .. } => source.exit_code(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message)
            | Self::Connection(message)
            | Self::Rpc(message)
            | Self::Timeout(message) => f.write_str(message),
            Self::Recovery { source, .. } => source.fmt(f),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Recovery { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_preserves_known_delivery_state_ids_and_exit_code() {
        let error = CliError::Timeout("wait timed out".into())
            .with_recovery(serde_json::json!({
                "clientInputId": "input-1", "deliveryState": "accepted", "stage": "observe",
            }))
            .with_recovery(serde_json::json!({
                "sessionId": "session-1", "deliveryState": "not_submitted", "stage": "configure",
            }));
        assert_eq!(error.exit_code(), ExitCode::Timeout);
        assert_eq!(error.to_string(), "wait timed out");
        let recovery = error.recovery().unwrap();
        assert_eq!(recovery["sessionId"], "session-1");
        assert_eq!(recovery["clientInputId"], "input-1");
        assert_eq!(recovery["deliveryState"], "accepted");
        assert_eq!(recovery["stage"], "observe");
    }

    #[test]
    fn notification_errors_preserve_existing_rpc_classification() {
        for message in [
            "WebSocket writer is closed",
            "WebSocket writer stopped before flushing notification",
            "WebSocket write failed: connection reset",
            "WebSocket write failed: operation timed out",
            "remote service reported a timeout",
        ] {
            let error = CliError::from_notification(anyhow::anyhow!(message));
            assert!(matches!(error, CliError::Rpc(_)));
            assert_eq!(error.exit_code().as_i32(), 3);
            assert_eq!(error.to_string(), message);
        }
    }
}
