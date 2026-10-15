//! Opt-in backend acceptance probe, separate from cache-hit/usage diagnostics.
#![cfg(all(feature = "api", not(target_arch = "wasm32")))]

use reqwest::blocking::Client;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// The CLI accumulator requires response metadata before any semantic output,
// including an empty terminal completion.
const CAPTURE_RESPONSE: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"capture-fixture\"}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
);

#[derive(Clone, Copy, Debug)]
enum Case {
    Baseline,
    Options,
    Breakpoint,
    Combined,
}

impl Case {
    fn name(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Options => "options_only",
            Self::Breakpoint => "breakpoint_only",
            Self::Combined => "combined",
        }
    }

    fn parameters(self) -> &'static [&'static str] {
        match self {
            Self::Baseline => &[],
            Self::Options => &["prompt_cache_options"],
            Self::Breakpoint => &["prompt_cache_breakpoint"],
            Self::Combined => &["prompt_cache_options", "prompt_cache_breakpoint"],
        }
    }

    fn body(self, baseline: &Value) -> Value {
        let mut body = baseline.clone();
        if matches!(self, Self::Options | Self::Combined) {
            body["prompt_cache_options"] = json!({"mode": "explicit"});
        }
        if matches!(self, Self::Breakpoint | Self::Combined) {
            // Mark the stable prefix, not the final user question or request root.
            body["input"][0]["content"][0]["prompt_cache_breakpoint"] = json!({"mode": "explicit"});
        }
        body
    }
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Accepted,
    Rejected(&'static str),
    Inconclusive,
}

#[derive(Clone, Copy)]
enum Expectation {
    Accept,
    Reject,
}

impl Expectation {
    fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "accept" => Ok(Self::Accept),
            "reject" => Ok(Self::Reject),
            _ => Err("QMT_TEST_CODEX_CACHE_EXPECT must be accept or reject"),
        }
    }

    fn matches(self, verdict: &Verdict) -> bool {
        matches!(
            (self, verdict),
            (Self::Accept, Verdict::Accepted) | (Self::Reject, Verdict::Rejected(_))
        )
    }
}

// Never print backend messages: even error payloads may echo credentials or prompts.
// Only return an allowlisted parameter name for an explicit validation rejection.
fn rejection(error: &Value, case: Case) -> Verdict {
    let error = error
        .get("error")
        .or_else(|| error.get("detail").filter(|detail| detail.is_object()))
        .unwrap_or(error);
    let message = error["message"]
        .as_str()
        .or_else(|| error["detail"].as_str())
        .or_else(|| error.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let code = error["code"].as_str().unwrap_or("");
    let kind = error["type"].as_str().unwrap_or("");
    if matches!(
        code,
        "rate_limit_exceeded" | "usage_limit_reached" | "insufficient_quota" | "invalid_api_key"
    ) || matches!(
        kind,
        "rate_limit_error" | "authentication_error" | "server_error"
    ) {
        return Verdict::Inconclusive;
    }
    let validation = [
        "unsupported",
        "not supported",
        "does not support",
        "must be",
        "unknown",
        "unrecognized",
        "invalid",
        "not allowed",
        "unexpected",
        "extra inputs",
    ]
    .iter()
    .any(|word| message.contains(word))
        || matches!(kind, "invalid_request_error" | "validation_error")
        || matches!(
            code,
            "unsupported_parameter" | "unknown_parameter" | "invalid_parameter" | "invalid_value"
        );
    let param = error["param"].as_str().unwrap_or("");
    if validation {
        for &field in case.parameters() {
            if param == field
                || param.starts_with(&format!("{field}."))
                || (field == "prompt_cache_breakpoint" && param.contains("prompt_cache_breakpoint"))
                || message.contains(field)
            {
                return Verdict::Rejected(match field {
                    "prompt_cache_options"
                        if param.starts_with("prompt_cache_options.mode")
                            || message.contains("prompt_cache_options.mode") =>
                    {
                        "prompt_cache_options.mode"
                    }
                    _ => field,
                });
            }
        }
    }
    Verdict::Inconclusive
}

fn classify(status: u16, body: &[u8], case: Case) -> Verdict {
    if matches!(status, 400 | 422) {
        return serde_json::from_slice(body)
            .map(|error| rejection(&error, case))
            .unwrap_or(Verdict::Inconclusive);
    }
    if !(200..300).contains(&status) {
        return Verdict::Inconclusive;
    }
    for line in String::from_utf8_lossy(body).lines() {
        let Some(data) = line.trim().strip_prefix("data: ") else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match event["type"].as_str() {
            Some("response.failed") => return rejection(&event["response"], case),
            Some("error") => return rejection(&event, case),
            Some("response.incomplete") => return Verdict::Inconclusive,
            Some("response.completed") if event["response"].is_object() => {
                return if event["response"]["end_turn"] == false {
                    Verdict::Inconclusive
                } else {
                    Verdict::Accepted
                };
            }
            _ => {}
        }
    }
    Verdict::Inconclusive
}

// Capture exactly one plugin-generated request locally. This avoids duplicating
// OAuth/keychain logic and avoids duplicate `input` keys via extra_body flattening.
// Credentials stay in memory; they are sent only to the fixed ChatGPT endpoint.
fn receive_request(listener: TcpListener) -> Result<(HeaderMap, Value), &'static str> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let (mut socket, _) = loop {
        match listener.accept() {
            Ok(socket) => break socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err("CLI did not reach loopback capture within 60 seconds");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return Err("loopback accept failed"),
        }
    };
    socket
        .set_nonblocking(false)
        .map_err(|_| "configure capture socket")?;
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(&mut socket);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|_| "read request line")?;
    if !line.starts_with("POST /responses HTTP/1.1") {
        return Err("unexpected plugin request path");
    }
    let mut headers = HeaderMap::new();
    let mut length = None;
    let mut header_bytes = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).map_err(|_| "read header")?;
        header_bytes += line.len();
        if header_bytes > 64 * 1024 || line.is_empty() {
            return Err("invalid or excessive request headers");
        }
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').ok_or("invalid request header")?;
        let name = name.to_ascii_lowercase();
        if name == "content-length" {
            length = value.trim().parse::<usize>().ok();
        }
        if [
            "authorization",
            "chatgpt-account-id",
            "openai-beta",
            "originator",
            "content-type",
        ]
        .contains(&name.as_str())
        {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| "invalid header name")?,
                value.trim().parse().map_err(|_| "invalid header value")?,
            );
        }
    }
    let length = length
        .filter(|len| *len <= 1024 * 1024)
        .ok_or("missing or excessive content length")?;
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| "read request body")?;
    let body = serde_json::from_slice(&bytes).map_err(|_| "invalid plugin request JSON")?;
    // A synthetic completion lets the CLI exit normally; it is NOT an acceptance verdict.
    let response = CAPTURE_RESPONSE;
    write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response)
            .map_err(|_| "write capture response")?;
    Ok((headers, body))
}

fn capture_request(binary: &str, config: &str, model: &str) -> (HeaderMap, Value) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback capture");
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}/", listener.local_addr().unwrap());
    let capture = std::thread::spawn(move || receive_request(listener));
    let output = Command::new(binary)
        .args(["--provider-config", config, "-p", "codex", "--model", model])
        .args([
            "-s",
            "Fixed cache acceptance prefix.",
            "-o",
            "stream=true",
            "-o",
            "timeout_seconds=60",
        ])
        .args([
            "-o",
            &format!("base_url={base_url}"),
            "-o",
            "extra_body={}",
            "Reply with OK only.",
        ])
        .env("QMT_NO_TELEMETRY", "1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output();
    let request = capture
        .join()
        .expect("capture thread panicked")
        .expect("capture plugin request");
    assert!(
        output.is_ok_and(|output| output.status.success()),
        "CLI capture failed; verify CLI/plugin builds and OAuth (output withheld)"
    );
    assert!(
        request.0.contains_key("authorization"),
        "plugin supplied no authorization"
    );
    assert!(
        request.0.contains_key("chatgpt-account-id"),
        "plugin supplied no account ID"
    );
    assert_eq!(
        request.1["model"].as_str(),
        Some(model),
        "plugin changed the selected model"
    );
    assert_eq!(request.1["stream"], true, "probe requires SSE responses");
    assert_eq!(
        request.1["input"][0]["content"][0]["type"], "input_text",
        "missing stable input-text prefix"
    );
    assert!(
        request.1["input"][0]["content"][0]["text"].is_string(),
        "missing stable text prefix"
    );
    assert!(
        request.1.get("prompt_cache_options").is_none(),
        "baseline contains cache options"
    );
    assert!(
        !request.1.to_string().contains("prompt_cache_breakpoint"),
        "baseline contains a breakpoint"
    );
    request
}

#[test]
#[ignore = "live Codex parameter acceptance: requires explicit opt-in, CLI/plugin and OAuth; consumes quota"]
fn live_codex_cache_parameter_acceptance() {
    assert_eq!(
        std::env::var("QMT_RUN_LIVE_CODEX").as_deref(),
        Ok("1"),
        "Set QMT_RUN_LIVE_CODEX=1 to authorize live requests"
    );
    let expected = Expectation::parse(
        &std::env::var("QMT_TEST_CODEX_CACHE_EXPECT").unwrap_or_else(|_| "accept".into()),
    )
    .unwrap();
    let binary = std::env::var("QMT_TEST_QMT_BIN").expect("set QMT_TEST_QMT_BIN");
    let config = std::env::var("QMT_TEST_PROVIDER_CONFIG").expect("set QMT_TEST_PROVIDER_CONFIG");
    let model = std::env::var("QMT_TEST_CODEX_MODEL").unwrap_or_else(|_| "gpt-5.6-sol".into());
    let (headers, baseline) = capture_request(&binary, &config, &model);
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build HTTP client");
    let mut failures = Vec::new();
    for case in [
        Case::Baseline,
        Case::Options,
        Case::Breakpoint,
        Case::Combined,
    ] {
        let result = client
            .post("https://chatgpt.com/backend-api/codex/responses")
            .headers(headers.clone())
            .json(&case.body(&baseline))
            .send();
        let (status, verdict) = match result {
            Ok(response) => {
                let status = response.status().as_u16();
                let mut bytes = Vec::new();
                let verdict = if response
                    .take(4 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .is_ok()
                    && bytes.len() <= 4 * 1024 * 1024
                {
                    classify(status, &bytes, case)
                } else {
                    Verdict::Inconclusive
                };
                (Some(status), verdict)
            }
            Err(_) => (None, Verdict::Inconclusive),
        };
        // Debug formatting of Verdict contains only controlled enum/parameter names.
        eprintln!(
            "model={} case={} http={status:?} result={verdict:?}",
            serde_json::to_string(&model).unwrap(),
            case.name()
        );
        if matches!(case, Case::Baseline) {
            assert_eq!(
                verdict,
                Verdict::Accepted,
                "baseline failed; parameter support cannot be determined"
            );
        } else if !expected.matches(&verdict) {
            failures.push(case.name());
        }
    }
    assert!(
        failures.is_empty(),
        "cache parameter expectation not met for {failures:?}; rejection is not a cache-hit verdict, and Inconclusive is not evidence of unsupported parameters"
    );
}

#[test]
fn capture_response_satisfies_the_codex_parser_and_cli_accumulator() {
    use querymt::chat::http::HTTPChatProvider;
    use querymt::chat::{ChatStreamAccumulator, ChatStreamFinish};

    let provider: qmt_codex::Codex = serde_json::from_value(json!({
        "api_key": "offline-fixture",
        "model": "offline-fixture"
    }))
    .unwrap();
    // Exercise the same parser and accumulator as the CLI, not just the
    // acceptance classifier (which intentionally only checks terminal status).
    let individual_frames: Vec<&str> = CAPTURE_RESPONSE.split_inclusive("\n\n").collect();
    // Transports may deliver multiple SSE frames together or one at a time.
    for frames in [vec![CAPTURE_RESPONSE], individual_frames] {
        let mut parser = provider.chat_stream_parser().unwrap();
        let mut accumulator = ChatStreamAccumulator::default();
        for frame in frames {
            for chunk in parser.parse_chunk(frame.as_bytes()).unwrap() {
                accumulator.push(&chunk).unwrap();
            }
        }
        for chunk in parser.finish().unwrap() {
            accumulator.push(&chunk).unwrap();
        }
        assert!(matches!(
            accumulator.finish(),
            ChatStreamFinish::Completed(_)
        ));
    }
}

#[test]
fn loopback_capture_preserves_body_and_only_forwards_allowlisted_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let capture = std::thread::spawn(move || receive_request(listener));
    let body =
        json!({"input": [{"role":"user", "content":[{"type":"input_text", "text":"fixture"}]}]});
    let response = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .post(endpoint)
        .header("authorization", "Bearer offline-fixture")
        .header("chatgpt-account-id", "offline-account")
        .header("x-do-not-forward", "private-fixture")
        .json(&body)
        .send();
    let (headers, captured) = capture.join().unwrap().unwrap();
    let response = response.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(headers["authorization"], "Bearer offline-fixture");
    assert_eq!(headers["chatgpt-account-id"], "offline-account");
    assert!(!headers.contains_key("host"));
    assert!(!headers.contains_key("content-length"));
    assert!(!headers.contains_key("x-do-not-forward"));
    assert_eq!(captured, body);
    let reply = response.bytes().unwrap();
    assert_eq!(classify(200, &reply, Case::Baseline), Verdict::Accepted);
}

#[test]
fn request_variants_change_only_the_target_fields() {
    let baseline = json!({"model":"fixture", "stream":true, "store":false, "input":[{"role":"user", "content":[{"type":"input_text", "text":"stable prefix"}]}]});
    for case in [
        Case::Baseline,
        Case::Options,
        Case::Breakpoint,
        Case::Combined,
    ] {
        let body = case.body(&baseline);
        // Round-trip the final wire body, not a second flattened input override.
        let mut wire: Value = serde_json::from_slice(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(
            wire.get("prompt_cache_options").is_some(),
            matches!(case, Case::Options | Case::Combined)
        );
        assert_eq!(
            wire["input"][0]["content"][0]
                .get("prompt_cache_breakpoint")
                .is_some(),
            matches!(case, Case::Breakpoint | Case::Combined)
        );
        assert!(wire.get("prompt_cache_breakpoint").is_none());
        if let Some(options) = wire.as_object_mut().unwrap().remove("prompt_cache_options") {
            assert_eq!(options, json!({"mode":"explicit"}));
        }
        if let Some(point) = wire["input"][0]["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("prompt_cache_breakpoint")
        {
            assert_eq!(point, json!({"mode":"explicit"}));
        }
        assert_eq!(wire, baseline);
    }
}

#[test]
fn verdicts_require_completion_or_explicit_parameter_rejection() {
    let rejected = br#"{"error":{"message":"Unsupported parameter: prompt_cache_options.mode; secret fixture", "param":"prompt_cache_options.mode"}}"#;
    assert_eq!(
        classify(400, rejected, Case::Options),
        Verdict::Rejected("prompt_cache_options.mode")
    );
    assert_eq!(
        classify(422, rejected, Case::Combined),
        Verdict::Rejected("prompt_cache_options.mode")
    );
    for status in [401, 403, 429, 500, 503] {
        assert_eq!(
            classify(status, rejected, Case::Options),
            Verdict::Inconclusive
        );
    }
    assert_eq!(
        classify(
            400,
            br#"{"detail":"Unsupported parameter: prompt_cache_options.mode"}"#,
            Case::Options
        ),
        Verdict::Rejected("prompt_cache_options.mode")
    );
    assert_eq!(
        classify(400, br#"{"error":{"type":"rate_limit_error","message":"Invalid quota for prompt_cache_options"}}"#, Case::Options),
        Verdict::Inconclusive
    );
    assert_eq!(
        classify(400, rejected, Case::Breakpoint),
        Verdict::Inconclusive
    );
    assert_eq!(
        classify(
            400,
            br#"{"error":{"message":"invalid model"}}"#,
            Case::Options
        ),
        Verdict::Inconclusive
    );
    assert_eq!(
        classify(200, b"data: [DONE]\n\n", Case::Options),
        Verdict::Inconclusive
    );
    assert_eq!(
        classify(
            200,
            b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            Case::Options
        ),
        Verdict::Accepted
    );
    assert_eq!(classify(200, b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"Unknown parameter prompt_cache_breakpoint\"}}}\n\n", Case::Breakpoint), Verdict::Rejected("prompt_cache_breakpoint"));
    assert_eq!(
        classify(
            200,
            b"data: {\"type\":\"response.incomplete\"}\n\n",
            Case::Combined
        ),
        Verdict::Inconclusive
    );
    assert!(Expectation::Accept.matches(&Verdict::Accepted));
    assert!(!Expectation::Accept.matches(&Verdict::Rejected("prompt_cache_options")));
    assert!(Expectation::Reject.matches(&Verdict::Rejected("prompt_cache_options")));
    assert!(!Expectation::Reject.matches(&Verdict::Accepted));
    assert!(!Expectation::Reject.matches(&Verdict::Inconclusive));
    assert!(Expectation::parse("invalid").is_err());
    assert!(!format!("{:?}", classify(400, rejected, Case::Options)).contains("secret fixture"));
}
