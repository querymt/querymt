use super::*;
use crate::{BridgeConfig, QmcpServer, policy::PermissionPolicy, transport};
use reqwest::{Client, redirect::Policy};
use serde_json::Value;

const RESOURCE: &str = "https://mcp.example.com/mcp";
const REDIRECT: &str = "https://chatgpt.com/connector_platform_oauth_redirect";
const VERIFIER: &str = "verifier-verifier-verifier-verifier-verifier";

fn keys(byte: u8) -> OAuthKeys {
    OAuthKeys::new(vec![byte; 32], "approval-password".into()).unwrap()
}

fn challenge() -> String {
    B64.encode(Sha256::digest(VERIFIER.as_bytes()))
}

struct App {
    base: String,
    client: Client,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for App {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start(auth: LocalOAuth) -> App {
    let server =
        QmcpServer::new(BridgeConfig::new("127.0.0.1:1", false, PermissionPolicy::Cancel).unwrap())
            .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap();
    let router = transport::http_router_with_local_oauth(server, bind, auth, &[]).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    App {
        base: format!("http://{bind}"),
        client: Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .build()
            .unwrap(),
        task,
    }
}

impl App {
    async fn register(&self) -> String {
        let response = self
            .client
            .post(format!("{}/register", self.base))
            .json(&json!({"redirect_uris": [REDIRECT], "client_name": "ChatGPT"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        response.json::<Value>().await.unwrap()["client_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn authorize_form(&self, client_id: &str) -> Vec<(&'static str, String)> {
        vec![
            ("response_type", "code".into()),
            ("client_id", client_id.to_owned()),
            ("redirect_uri", REDIRECT.into()),
            ("code_challenge", challenge()),
            ("code_challenge_method", "S256".into()),
            ("state", "xyz".into()),
            ("resource", RESOURCE.into()),
        ]
    }

    /// Submit the consent form and return the redirect query parameters.
    async fn submit(&self, form: &[(&str, String)]) -> (StatusCode, HashMap<String, String>) {
        let response = self
            .client
            .post(format!("{}/authorize", self.base))
            .form(form)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let query = response
            .headers()
            .get(LOCATION)
            .map(|location| {
                Url::parse(location.to_str().unwrap())
                    .unwrap()
                    .query_pairs()
                    .into_owned()
                    .collect()
            })
            .unwrap_or_default();
        (status, query)
    }

    async fn token(&self, form: &[(&str, &str)]) -> (StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}/token", self.base))
            .form(form)
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn mcp(&self, token: Option<&str>) -> reqwest::Response {
        let mut request = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.unwrap()
    }
}

#[tokio::test]
async fn registration_consent_token_refresh_and_mcp_access() {
    let app = start(LocalOAuth::new(RESOURCE, keys(1)).unwrap()).await;

    let metadata: Value = app
        .client
        .get(format!(
            "{}/.well-known/oauth-authorization-server",
            app.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["issuer"], "https://mcp.example.com");
    assert_eq!(
        metadata["registration_endpoint"],
        "https://mcp.example.com/register"
    );
    assert_eq!(
        metadata["code_challenge_methods_supported"],
        json!(["S256"])
    );
    let resource: Value = app
        .client
        .get(format!(
            "{}/.well-known/oauth-protected-resource/mcp",
            app.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resource["resource"], RESOURCE);
    assert_eq!(
        resource["authorization_servers"],
        json!(["https://mcp.example.com"])
    );

    let unauthorized = app.mcp(None).await;
    assert_eq!(unauthorized.status(), 401);
    assert!(
        unauthorized.headers()[WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .contains("https://mcp.example.com/.well-known/oauth-protected-resource/mcp")
    );

    let client_id = app.register().await;
    let form = app.authorize_form(&client_id);
    let page = app
        .client
        .get(format!("{}/authorize", app.base))
        .query(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("ChatGPT"));

    let mut wrong = form.clone();
    wrong.extend([("decision", "approve".into()), ("password", "nope".into())]);
    assert_eq!(app.submit(&wrong).await.0, StatusCode::UNAUTHORIZED);

    let mut approve = form.clone();
    approve.extend([
        ("decision", "approve".into()),
        ("password", "approval-password".into()),
    ]);
    let (status, query) = app.submit(&approve).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(query["state"], "xyz");
    assert_eq!(query["iss"], "https://mcp.example.com");
    let code = &query["code"];

    let exchange = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("client_id", client_id.as_str()),
        ("redirect_uri", REDIRECT),
        ("code_verifier", VERIFIER),
        ("resource", RESOURCE),
    ];
    let (status, tokens) = app.token(&exchange).await;
    assert_eq!(status, 200, "{tokens}");
    assert_eq!(app.token(&exchange).await.1["error"], "invalid_grant");

    let access = tokens["access_token"].as_str().unwrap();
    assert_eq!(app.mcp(Some(access)).await.status(), 200);

    let refresh = tokens["refresh_token"].as_str().unwrap();
    let (status, refreshed) = app
        .token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", client_id.as_str()),
        ])
        .await;
    assert_eq!(status, 200, "{refreshed}");
    let access = refreshed["access_token"].as_str().unwrap();
    assert_eq!(app.mcp(Some(access)).await.status(), 200);
    // A refresh token is not an access token.
    assert_eq!(app.mcp(Some(refresh)).await.status(), 401);
}

#[tokio::test]
async fn invalid_requests_are_rejected() {
    let auth = LocalOAuth::new(RESOURCE, keys(1)).unwrap();
    let app = start(auth.clone()).await;
    let client_id = app.register().await;

    let bad_registration = app
        .client
        .post(format!("{}/register", app.base))
        .json(&json!({"redirect_uris": ["http://evil.example.com/cb"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_registration.status(), 400);

    let mut unregistered = app.authorize_form(&client_id);
    unregistered[2].1 = "https://evil.example.com/cb".into();
    assert_eq!(app.submit(&unregistered).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        app.submit(&app.authorize_form("forged.client")).await.0,
        StatusCode::BAD_REQUEST
    );

    let mut wrong_resource = app.authorize_form(&client_id);
    wrong_resource[6].1 = "https://other.example.com/mcp".into();
    assert_eq!(
        app.submit(&wrong_resource).await.1["error"],
        "invalid_target"
    );

    let mut deny = app.authorize_form(&client_id);
    deny.push(("decision", "deny".into()));
    let (_, query) = app.submit(&deny).await;
    assert_eq!(query["error"], "access_denied");
    assert_eq!(query["iss"], "https://mcp.example.com");

    let mut approve = app.authorize_form(&client_id);
    approve.extend([
        ("decision", "approve".into()),
        ("password", "approval-password".into()),
    ]);
    let (_, query) = app.submit(&approve).await;
    let (status, error) = app
        .token(&[
            ("grant_type", "authorization_code"),
            ("code", query["code"].as_str()),
            ("client_id", client_id.as_str()),
            ("redirect_uri", REDIRECT),
            ("code_verifier", "wrong-verifier"),
        ])
        .await;
    assert_eq!(
        (status.as_u16(), error["error"].as_str()),
        (400, Some("invalid_grant"))
    );

    let expired = auth.sign(&Claims::Access {
        aud: RESOURCE.into(),
        exp: 1,
    });
    let foreign = LocalOAuth::new(RESOURCE, keys(2))
        .unwrap()
        .sign(&Claims::Access {
            aud: RESOURCE.into(),
            exp: now() + 60,
        });
    for token in [expired.as_str(), foreign.as_str(), "garbage"] {
        let response = app.mcp(Some(token)).await;
        assert_eq!(response.status(), 401);
        assert!(
            response.headers()[WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .contains("invalid_token")
        );
    }
}

#[test]
fn clients_and_tokens_survive_a_restart_with_the_same_keys() {
    let before = LocalOAuth::new(RESOURCE, keys(1)).unwrap();
    let after = LocalOAuth::new(RESOURCE, keys(1)).unwrap();
    let rotated = LocalOAuth::new(RESOURCE, keys(2)).unwrap();
    let token = before.sign(&Claims::Access {
        aud: RESOURCE.into(),
        exp: now() + 60,
    });
    assert!(after.accepts(&token));
    assert!(!rotated.accepts(&token));
    let client = before.sign(&Claims::Client {
        redirect_uris: vec![REDIRECT.into()],
        name: None,
    });
    assert!(matches!(after.verify(&client), Some(Claims::Client { .. })));
    assert!(rotated.verify(&client).is_none());
}

#[test]
fn resource_must_be_a_public_https_mcp_url() {
    for resource in [
        "http://mcp.example.com/mcp",
        "https://mcp.example.com/other",
        "https://mcp.example.com/mcp?x=1",
    ] {
        assert!(LocalOAuth::new(resource, keys(1)).is_err(), "{resource}");
    }
    let auth = LocalOAuth::new("https://mcp.example.com:8443/mcp", keys(1)).unwrap();
    assert_eq!(auth.public_host(), "mcp.example.com:8443");
    assert!(OAuthKeys::new(vec![0; 16], "pw".into()).is_err());
}
