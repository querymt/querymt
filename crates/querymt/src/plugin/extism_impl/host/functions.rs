use extism::{CurrentPlugin, UserData, Val};
use tracing::instrument;

#[cfg(feature = "http-client")]
use futures::StreamExt;
#[cfg(feature = "http-client")]
use http::header::ACCEPT;

#[cfg(feature = "http-client")]
#[instrument(name = "host_sse_connect", skip_all)]
pub(crate) fn sse_connect<T>(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    _user_data: UserData<T>,
) -> Result<(), extism::Error> {
    let url: String = plugin.memory_get_val(&inputs[0])?;

    log::debug!("sse_connect called for URL: {}", url);

    let _task = tokio::spawn(async move {
        let client = reqwest::Client::new();
        match client
            .get(&url)
            .header(ACCEPT, "text/event-stream")
            .send()
            .await
        {
            Ok(response) => {
                let mut stream = response.bytes_stream();
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(bytes) => {
                            let event = String::from_utf8_lossy(&bytes).to_string();
                            let _params = serde_json::to_vec(&event).unwrap();
                            // Note: Can't easily pass plugin reference into async block
                            log::debug!("SSE event received: {}", event);
                        }
                        Err(e) => {
                            log::error!("SSE stream error: {}", e);
                            break;
                        }
                    }
                }
            }
            Err(e) => log::error!("Failed to connect to SSE endpoint: {}", e),
        }
    });

    Ok(())
}

#[cfg(feature = "http-client")]
#[instrument(name = "host_reqwest_http", skip_all)]
pub(crate) fn reqwest_http(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<Vec<String>>, // allowed_hosts
) -> Result<(), extism::Error> {
    use crate::plugin::extism_impl::{SerializableHttpRequest, SerializableHttpResponse};

    // 1. Deserialize the http::Request from WASM memory
    let req_json: Vec<u8> = plugin.memory_get_val(&inputs[0])?;
    let ser_req: SerializableHttpRequest = serde_json::from_slice(&req_json)
        .map_err(|e| extism::Error::msg(format!("Failed to deserialize request: {}", e)))?;

    let http_req = ser_req.req;

    if let Ok(allowed) = user_data.get() {
        let allowed_guard = allowed.lock().unwrap();
        if let Some(host) = http_req.uri().host() {
            if !allowed_guard.is_empty() && !allowed_guard.iter().any(|h| h == host) {
                log::warn!("Blocked request to non-allowed host: {}", host);
                let error_resp = http::Response::builder()
                    .status(403)
                    .body(format!("Host '{}' not in allowlist", host).into_bytes())
                    .unwrap();

                let ser_resp = SerializableHttpResponse { resp: error_resp };
                let resp_json = serde_json::to_vec(&ser_resp)
                    .map_err(|e| extism::Error::msg(format!("Serialization error: {}", e)))?;
                let handle = plugin.memory_new(resp_json)?;
                outputs[0] = Val::I64(handle.offset as i64);
                return Ok(());
            }
        }
    }

    log::debug!("reqwest_http: {} {}", http_req.method(), http_req.uri());

    // 3. Convert http::Request to reqwest::Request and execute
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = tokio::runtime::Handle::current();
    let http_req_clone = http_req.clone();

    std::thread::spawn(move || {
        let res = handle.block_on(async move {
            let http_req = http_req_clone;
            let client = reqwest::Client::new();

            let method = reqwest::Method::from_bytes(http_req.method().as_str().as_bytes())
                .map_err(|e| format!("Invalid HTTP method: {}", e))?;
            let url = http_req.uri().to_string();

            let mut reqwest_req = client.request(method, &url);

            // Copy headers
            for (name, value) in http_req.headers() {
                if let Ok(val_str) = value.to_str() {
                    reqwest_req = reqwest_req.header(name.as_str(), val_str);
                }
            }

            // Add body
            let body = http_req.body().clone();
            if !body.is_empty() {
                reqwest_req = reqwest_req.body(body);
            }

            // Execute request
            match reqwest_req.send().await {
                Ok(reqwest_resp) => {
                    let status = reqwest_resp.status();
                    let version = reqwest_resp.version();
                    let headers = reqwest_resp.headers().clone();
                    let body = reqwest_resp
                        .bytes()
                        .await
                        .map_err(|e| format!("Failed to read response body: {}", e))?
                        .to_vec();

                    // Build http::Response
                    let mut builder = http::Response::builder().status(status).version(version);

                    for (name, value) in headers.iter() {
                        builder = builder.header(name, value);
                    }

                    builder
                        .body(body)
                        .map_err(|e| format!("Failed to build response: {}", e))
                }
                Err(e) => {
                    log::warn!("HTTP request failed: {}", e);
                    // Return 500 error response
                    http::Response::builder()
                        .status(500)
                        .body(format!("Request failed: {}", e).into_bytes())
                        .map_err(|e| format!("Failed to build error response: {}", e))
                }
            }
        });
        let _ = tx.send(res);
    });

    let http_resp = rx
        .recv()
        .map_err(|e| extism::Error::msg(format!("Channel error: {}", e)))?;

    let http_resp = http_resp.map_err(|e: String| extism::Error::msg(e))?;

    // 4. Serialize http::Response and write back to WASM memory
    let ser_resp = SerializableHttpResponse { resp: http_resp };
    let resp_json = serde_json::to_vec(&ser_resp)
        .map_err(|e| extism::Error::msg(format!("Failed to serialize response: {}", e)))?;

    let handle = plugin.memory_new(resp_json)?;
    outputs[0] = Val::I64(handle.offset as i64);

    Ok(())
}
