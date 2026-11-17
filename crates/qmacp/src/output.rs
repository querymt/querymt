use std::io::{self, Write};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};

use crate::error::{CliError, ExitCode};

pub fn write_json(value: &impl Serialize, pretty: bool) -> Result<()> {
    let text = if pretty {
        serde_json::to_string_pretty(value).context("serialize JSON")?
    } else {
        serde_json::to_string(value).context("serialize JSON")?
    };
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes()).context("write JSON")?;
    stdout.write_all(b"\n").context("write JSON newline")?;
    stdout.flush().context("flush JSON")
}

pub fn write_event(value: &impl Serialize) -> Result<()> {
    write_json(value, false)
}

pub fn write_error(error: &CliError, pretty: bool) -> Result<()> {
    let mut value = json!({
        "ok": false,
        "error": error.to_string(),
        "code": error.exit_code().as_i32(),
    });
    if let Some(recovery) = error.recovery() {
        value["recovery"] = recovery.clone();
    }
    write_json(&value, pretty)
}

pub fn ok(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("ok".to_string(), Value::Bool(true));
    }
    value
}

pub fn exit_for_stop_reason(stop_reason: &str) -> ExitCode {
    match stop_reason {
        "cancelled" => ExitCode::Timeout,
        _ => ExitCode::Ok,
    }
}
