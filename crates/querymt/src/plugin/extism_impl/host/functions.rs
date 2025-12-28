use extism::{CurrentPlugin, UserData, Val};
use futures::Stream;
use std::pin::Pin;
use tracing::instrument;

#[cfg(feature = "http-client")]
use futures::StreamExt;

pub(crate) struct HostState {
    pub allowed_hosts: Vec<String>,
    #[cfg(feature = "http-client")]
    pub http_streams: std::collections::HashMap<
        u64,
        Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>,
    >,
    pub next_stream_id: u64,
    pub yield_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    pub tokio_handle: tokio::runtime::Handle,
}

impl HostState {
    pub fn new(allowed_hosts: Vec<String>, tokio_handle: tokio::runtime::Handle) -> Self {
        Self {
            allowed_hosts,
            #[cfg(feature = "http-client")]
            http_streams: std::collections::HashMap::new(),
            next_stream_id: 1,
            yield_tx: None,
            tokio_handle,
        }
    }
}

pub(crate) fn qmt_http_request(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    #[cfg(feature = "http-client")]
    {
        reqwest_http(plugin, inputs, outputs, user_data)
    }
    #[cfg(not(feature = "http-client"))]
    {
        let _ = (plugin, inputs, outputs, user_data);
        Err(extism::Error::msg(
            "HTTP client feature not enabled in host",
        ))
    }
}

#[cfg(feature = "http-client")]
#[instrument(name = "host_reqwest_http", skip_all)]
pub(crate) fn reqwest_http(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    use crate::plugin::extism_impl::{SerializableHttpRequest, SerializableHttpResponse};

    let req_json: Vec<u8> = plugin.memory_get_val(&inputs[0])?;

    let ser_req: SerializableHttpRequest = serde_json::from_slice(&req_json).map_err(|e| {
        extism::Error::msg(format!(
            "Failed to deserialize request in reqwest_http: {}",
            e
        ))
    })?;

    let http_req = ser_req.req;
    let state = user_data.get()?;
    let handle_tokio = {
        let state_guard = state.lock().unwrap();
        if let Some(host) = http_req.uri().host() {
            if !state_guard.allowed_hosts.is_empty()
                && !state_guard.allowed_hosts.iter().any(|h| h == host)
            {
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
        state_guard.tokio_handle.clone()
    };

    let (tx, rx) = std::sync::mpsc::channel();
    let http_req_clone = http_req.clone();

    std::thread::spawn(move || {
        let res = handle_tokio.block_on(async move {
            let http_req = http_req_clone;
            let client = reqwest::Client::new();
            let method = reqwest::Method::from_bytes(http_req.method().as_str().as_bytes())
                .map_err(|e| format!("Invalid HTTP method: {}", e))?;
            let url = http_req.uri().to_string();
            let mut reqwest_req = client.request(method, &url);
            for (name, value) in http_req.headers() {
                if let Ok(val_str) = value.to_str() {
                    reqwest_req = reqwest_req.header(name.as_str(), val_str);
                }
            }
            let body = http_req.body().clone();
            if !body.is_empty() {
                reqwest_req = reqwest_req.body(body);
            }
            match reqwest_req.send().await {
                Ok(reqwest_resp) => {
                    let status = reqwest_resp.status();
                    let version = reqwest_resp.version();
                    let headers = reqwest_resp.headers().clone();
                    let body = reqwest_resp
                        .bytes()
                        .await
                        .map_err(|e| format!("{}", e))?
                        .to_vec();
                    let mut builder = http::Response::builder().status(status).version(version);
                    for (name, value) in headers.iter() {
                        builder = builder.header(name, value);
                    }
                    builder.body(body).map_err(|e| format!("{}", e))
                }
                Err(e) => http::Response::builder()
                    .status(500)
                    .body(format!("{}", e).into_bytes())
                    .map_err(|e| format!("{}", e)),
            }
        });
        let _ = tx.send(res);
    });

    let http_resp = rx
        .recv()
        .map_err(|e| extism::Error::msg(format!("{}", e)))?
        .map_err(|e| extism::Error::msg(e))?;
    let ser_resp = SerializableHttpResponse { resp: http_resp };
    let resp_json =
        serde_json::to_vec(&ser_resp).map_err(|e| extism::Error::msg(format!("{}", e)))?;
    let handle_resp = plugin.memory_new(resp_json)?;
    outputs[0] = Val::I64(handle_resp.offset as i64);
    Ok(())
}

pub(crate) fn qmt_http_stream_open(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    #[cfg(feature = "http-client")]
    {
        use crate::plugin::extism_impl::SerializableHttpRequest;

        let req_json: Vec<u8> = plugin.memory_get_val(&inputs[0])?;

        let ser_req: SerializableHttpRequest = serde_json::from_slice(&req_json).map_err(|e| {
            extism::Error::msg(format!(
                "Failed to deserialize request in qmt_http_stream_open: {}",
                e
            ))
        })?;
        let http_req = ser_req.req;
        let state = user_data.get()?;
        let handle_tokio = {
            let state_guard = state.lock().unwrap();
            if let Some(host) = http_req.uri().host() {
                if !state_guard.allowed_hosts.is_empty()
                    && !state_guard.allowed_hosts.iter().any(|h| h == host)
                {
                    return Err(extism::Error::msg(format!(
                        "Host '{}' not in allowlist",
                        host
                    )));
                }
            }
            state_guard.tokio_handle.clone()
        };
        let stream_res = handle_tokio.block_on(async move {
            let client = reqwest::Client::new();
            let method =
                reqwest::Method::from_bytes(http_req.method().as_str().as_bytes()).unwrap();
            let url = http_req.uri().to_string();
            let mut reqwest_req = client.request(method, &url);
            for (name, value) in http_req.headers() {
                if let Ok(val_str) = value.to_str() {
                    reqwest_req = reqwest_req.header(name.as_str(), val_str);
                }
            }
            let body = http_req.body().clone();
            if !body.is_empty() {
                reqwest_req = reqwest_req.body(body);
            }
            let resp = reqwest_req.send().await.map_err(|e| format!("{}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp
                    .text()
                    .await
                    .unwrap_or_else(|_| "could not read body".to_string());
                return Err(format!("HTTP Error {}: {}", status, body));
            }
            Ok::<_, String>(
                resp.bytes_stream()
                    .map(|result| result.map(|bytes| bytes.to_vec())),
            )
        });

        match stream_res {
            Ok(stream) => {
                let mut state_guard = state.lock().unwrap();
                let stream_id = state_guard.next_stream_id;
                state_guard.next_stream_id += 1;
                state_guard.http_streams.insert(stream_id, Box::pin(stream));

                let resp_json = serde_json::to_vec(&stream_id)
                    .map_err(|e| extism::Error::msg(format!("{}", e)))?;
                let handle = plugin.memory_new(resp_json)?;
                outputs[0] = Val::I64(handle.offset as i64);
            }
            Err(e) => {
                return Err(extism::Error::msg(format!("HTTP request failed: {}", e)));
            }
        }
        Ok(())
    }
    #[cfg(not(feature = "http-client"))]
    {
        let _ = (plugin, inputs, outputs, user_data);
        Err(extism::Error::msg(
            "HTTP client feature not enabled in host",
        ))
    }
}

pub(crate) fn qmt_http_stream_next(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    #[cfg(feature = "http-client")]
    {
        let stream_id_json: Vec<u8> = plugin.memory_get_val(&inputs[0])?;
        let stream_id: u64 = serde_json::from_slice(&stream_id_json).map_err(|e| {
            extism::Error::msg(format!("Failed to deserialize stream_id in next: {}", e))
        })?;

        let state = user_data.get()?;
        let (handle_tokio, stream_exists) = {
            let state_guard = state.lock().unwrap();
            (
                state_guard.tokio_handle.clone(),
                state_guard.http_streams.contains_key(&stream_id),
            )
        };
        if !stream_exists {
            return Err(extism::Error::msg(format!(
                "Stream {} not found",
                stream_id
            )));
        }

        let next_chunk = handle_tokio.block_on(async {
            let mut state_guard = state.lock().unwrap();
            let stream = state_guard.http_streams.get_mut(&stream_id).unwrap();
            stream.next().await
        });
        match next_chunk {
            Some(Ok(bytes)) => {
                let handle = plugin.memory_new(bytes.to_vec())?;
                outputs[0] = Val::I64(handle.offset as i64);
            }
            Some(Err(e)) => {
                return Err(extism::Error::msg(format!("Stream error: {}", e)));
            }
            None => {
                outputs[0] = Val::I64(0);
            }
        }
        Ok(())
    }
    #[cfg(not(feature = "http-client"))]
    {
        let _ = (plugin, inputs, outputs, user_data);
        Err(extism::Error::msg(
            "HTTP client feature not enabled in host",
        ))
    }
}

pub(crate) fn qmt_http_stream_close(
    _plugin: &mut CurrentPlugin,
    inputs: &[Val],
    _outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    #[cfg(feature = "http-client")]
    {
        let stream_id_json: Vec<u8> = _plugin.memory_get_val(&inputs[0])?;
        let stream_id: u64 = serde_json::from_slice(&stream_id_json).map_err(|e| {
            extism::Error::msg(format!("Failed to deserialize stream_id in close: {}", e))
        })?;

        let state = user_data.get()?;
        let mut state_guard = state.lock().unwrap();
        state_guard.http_streams.remove(&stream_id);
        Ok(())
    }
    #[cfg(not(feature = "http-client"))]
    {
        let _ = (inputs, _outputs, user_data);
        Ok(())
    }
}

pub(crate) fn qmt_yield_chunk(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    _outputs: &mut [Val],
    user_data: UserData<HostState>,
) -> Result<(), extism::Error> {
    let chunk_json: Vec<u8> = plugin.memory_get_val(&inputs[0])?;
    log::debug!("Host received qmt_yield_chunk: {} bytes", chunk_json.len());

    let state = user_data.get()?;
    let state_guard = state.lock().unwrap();
    if let Some(tx) = &state_guard.yield_tx {
        tx.send(chunk_json)
            .map_err(|e| extism::Error::msg(format!("Failed to yield chunk: {}", e)))?;
    } else {
        log::warn!("qmt_yield_chunk called but yield_tx is None");
    }
    Ok(())
}
