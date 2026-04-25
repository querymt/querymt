mod http_client {
    #[cfg(not(target_arch = "wasm32"))]
    pub mod imp {
        use crate::error::{LLMError, classify_http_status};
        use http::{Request, Response};
        use once_cell::sync::Lazy;
        use reqwest::Client;

        /// A single, global client, built once
        pub static CLIENT: Lazy<Client> = Lazy::new(Client::new);

        pub async fn call_outbound(req: Request<Vec<u8>>) -> Result<Response<Vec<u8>>, LLMError> {
            let client = &*CLIENT;

            let method = req
                .method()
                .as_str()
                .parse::<reqwest::Method>()
                .map_err(|e| LLMError::HttpError(e.to_string()))?;

            let mut rb = client.request(method, req.uri().to_string());

            for (name, value) in req.headers().iter() {
                let val_str = value
                    .to_str()
                    .map_err(|e| LLMError::HttpError(e.to_string()))?;
                rb = rb.header(name.as_str(), val_str);
            }

            let resp = rb.body(req.into_body()).send().await?;
            let status = resp.status();
            let headers = resp.headers().clone();
            let bytes = resp.bytes().await?.to_vec();

            if !status.is_success() {
                return Err(classify_http_status(status.as_u16(), &headers, &bytes));
            }

            let mut builder = Response::builder().status(status.as_u16());
            for (name, value) in headers.iter() {
                builder = builder.header(name.as_str(), value.as_bytes());
            }
            Ok(builder.body(bytes).unwrap())
        }

        pub async fn call_outbound_stream(
            req: Request<Vec<u8>>,
        ) -> Result<impl futures::Stream<Item = reqwest::Result<bytes::Bytes>>, LLMError> {
            let client = &*CLIENT;

            let method = req
                .method()
                .as_str()
                .parse::<reqwest::Method>()
                .map_err(|e| LLMError::HttpError(e.to_string()))?;

            let mut rb = client.request(method, req.uri().to_string());

            for (name, value) in req.headers().iter() {
                let val_str = value
                    .to_str()
                    .map_err(|e| LLMError::HttpError(e.to_string()))?;
                rb = rb.header(name.as_str(), val_str);
            }

            let resp = rb.body(req.into_body()).send().await?;
            let status = resp.status();
            if !status.is_success() {
                let headers = resp.headers().clone();
                let bytes = resp.bytes().await?.to_vec();
                return Err(classify_http_status(status.as_u16(), &headers, &bytes));
            }
            Ok(resp.bytes_stream())
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub mod imp {
        use crate::error::LLMError;
        use http::{Request, Response};

        pub async fn call_outbound(_req: Request<Vec<u8>>) -> Result<Response<Vec<u8>>, LLMError> {
            Err(LLMError::InvalidRequest("".into()))
        }

        pub async fn call_outbound_stream(
            _req: Request<Vec<u8>>,
        ) -> Result<futures::stream::Empty<reqwest::Result<bytes::Bytes>>, LLMError> {
            Err(LLMError::InvalidRequest("".into()))
        }
    }
}

pub use http_client::imp::{call_outbound, call_outbound_stream};
