use super::connection::{send_error, send_message};
use super::messages::UiServerMessage;
use super::ServerState;
use crate::acp::protocol::ExtRequest;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;

pub async fn handle_querymt_models(
    state: &ServerState,
    refresh: bool,
    tx: &mpsc::Sender<String>,
) {
    let method = if refresh { "querymt/refreshModels" } else { "querymt/models" };
    let params = if refresh {
        serde_json::json!({})
    } else {
        serde_json::json!({})
    };
    let Some(raw) = serde_json::value::RawValue::from_string(params.to_string()).ok() else {
        let _ = send_error(tx, "Failed to encode querymt models request".to_string()).await;
        return;
    };

    let req = ExtRequest::new(method, Arc::from(raw));
    match state.agent.ext_method(req).await {
        Ok(resp) => match serde_json::from_str::<Value>(resp.0.get()) {
            Ok(value) => {
                let models = value
                    .get("models")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                let meta = value.get("meta").cloned();
                let msg = if refresh {
                    UiServerMessage::QuerymtRefreshModelsResult { models, meta }
                } else {
                    UiServerMessage::QuerymtModelsResult { models, meta }
                };
                let _ = send_message(tx, msg).await;
            }
            Err(err) => {
                let _ = send_error(tx, format!("Failed to decode {} response: {}", method, err)).await;
            }
        },
        Err(err) => {
            let _ = send_error(tx, format!("{} failed: {}", method, err)).await;
        }
    }
}

pub async fn handle_querymt_model_info(
    state: &ServerState,
    models: Vec<Value>,
    tx: &mpsc::Sender<String>,
) {
    let params = serde_json::json!({ "models": models });
    let Some(raw) = serde_json::value::RawValue::from_string(params.to_string()).ok() else {
        let _ = send_error(tx, "Failed to encode querymt model info request".to_string()).await;
        return;
    };

    let req = ExtRequest::new("querymt/modelInfo", Arc::from(raw));
    match state.agent.ext_method(req).await {
        Ok(resp) => match serde_json::from_str::<Value>(resp.0.get()) {
            Ok(value) => {
                let models = value.get("models").cloned().unwrap_or(Value::Null);
                let _ = send_message(tx, UiServerMessage::QuerymtModelInfoResult { models }).await;
            }
            Err(err) => {
                let _ = send_error(tx, format!("Failed to decode querymt/modelInfo response: {}", err)).await;
            }
        },
        Err(err) => {
            let _ = send_error(tx, format!("querymt/modelInfo failed: {}", err)).await;
        }
    }
}
