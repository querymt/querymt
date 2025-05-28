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

            // Native path: turn http::Request into reqwest and back
            let method = req
                .method()
                .as_str()
                .parse::<reqwest::Method>()
                .map_err(|e| Box::<dyn Error>::try_from(e).unwrap())?;

            let mut rb = client.request(method, req.uri().to_string());

            // propagate headers
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
                // value.as_bytes() is &[u8]; builder.header accepts either &str or &[u8]
                builder = builder.header(name.as_str(), value.as_bytes());
            }
            Ok(builder.body(bytes).unwrap())
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub mod imp {
        use http::{Request, Response};
        //        use spin_sdk::http::{send, Request as SpinReq, Response as SpinResp};
        use std::error::Error;

        use crate::error::LLMError;

        pub async fn call_outbound(
            req: Request<Vec<u8>>,
        ) -> Result<Response<Vec<u8>>, Box<dyn Error>> {
            /*

            // Convert http::Request<Vec<u8>> → spin_sdk::http::Request
            let mut spin_req = SpinReq::builder()
                .method(req.method().clone())
                .uri(req.uri().to_string());
            for (k, v) in req.headers().iter() {
                spin_req = spin_req.header(k, v.to_str()?);
            }
            let spin_req = spin_req.body(req.into_body()).unwrap();

            // Perform the outbound HTTP in the Spin host
            let spin_resp: SpinResp = send(spin_req).await?;

            // Convert back to http::Response<Vec<u8>>
            let mut builder = Response::builder().status(spin_resp.status());
            for (k, v) in spin_resp.headers() {
                builder = builder.header(k.as_str(), v.as_str());
            }
            Ok(builder.body(spin_resp.body().to_vec()).unwrap())
            */
            Err(Box::new(LLMError::InvalidRequest("".into())))
        }
    }
}

pub use http_client::imp::call_outbound;
