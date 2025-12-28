mod http_client {
    #[cfg(not(target_arch = "wasm32"))]
    pub mod imp {
        use http::{Request, Response};
        use once_cell::sync::Lazy;
        use reqwest::Client;
        use std::error::Error;

        /// A single, global client, built once
        pub static CLIENT: Lazy<Client> = Lazy::new(Client::new);

        pub async fn call_outbound(
            req: Request<Vec<u8>>,
        ) -> Result<Response<Vec<u8>>, Box<dyn Error>> {
            let client = &*CLIENT;

            let method = req
                .method()
                .as_str()
                .parse::<reqwest::Method>()
                .map_err(Box::<dyn Error>::from)?;

            let mut rb = client.request(method, req.uri().to_string());

            for (name, value) in req.headers().iter() {
                let val_str = value.to_str()?;
                rb = rb.header(name.as_str(), val_str);
            }

            let resp = rb.body(req.into_body()).send().await?.error_for_status()?;

            let status = resp.status();
            let headers = resp.headers().clone();
            let bytes = resp.bytes().await?.to_vec();

            let mut builder = Response::builder().status(status.as_u16());
            for (name, value) in headers.iter() {
                builder = builder.header(name.as_str(), value.as_bytes());
            }
            Ok(builder.body(bytes).unwrap())
        }

        pub async fn call_outbound_stream(
            req: Request<Vec<u8>>,
        ) -> Result<impl futures::Stream<Item = reqwest::Result<bytes::Bytes>>, Box<dyn Error>>
        {
            let client = &*CLIENT;

            let method = req
                .method()
                .as_str()
                .parse::<reqwest::Method>()
                .map_err(Box::<dyn Error>::from)?;

            let mut rb = client.request(method, req.uri().to_string());

            for (name, value) in req.headers().iter() {
                let val_str = value.to_str()?;
                rb = rb.header(name.as_str(), val_str);
            }

            let resp = rb.body(req.into_body()).send().await?.error_for_status()?;
            Ok(resp.bytes_stream())
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub mod imp {
        use crate::error::LLMError;
        use http::{Request, Response};
        use std::error::Error;

        pub async fn call_outbound(
            _req: Request<Vec<u8>>,
        ) -> Result<Response<Vec<u8>>, Box<dyn Error>> {
            Err(Box::new(LLMError::InvalidRequest("".into())))
        }

        pub async fn call_outbound_stream(
            _req: Request<Vec<u8>>,
        ) -> Result<futures::stream::Empty<reqwest::Result<bytes::Bytes>>, Box<dyn Error>> {
            Err(Box::new(LLMError::InvalidRequest("".into())))
        }
    }
}

pub use http_client::imp::{call_outbound, call_outbound_stream};
