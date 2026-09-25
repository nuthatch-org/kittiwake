use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::{Address, keccak256};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures::future::BoxFuture;
use kittiwake_cockpit::agent::Agent;
use kittiwake_cockpit::auth::{Auth, AuthError, ChainOperators, Operators};
use kittiwake_cockpit::{AppState, app};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{any, body_partial_json, header as has_header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORIGIN: &str = "http://localhost:3000";
// Fixed test keys. Never funded, never used anywhere but here.
const INDEXER_KEY: &str = "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";
const OPERATOR_KEY: &str = "ae6ae8e5ccbfb04590405997ee2d52d2b330726137b875053c36d94e974d162f";
const STRANGER_KEY: &str = "0dbbe8e4ae425a6d2687f1a7e3ba17bc98c673636790f1b8ad91193c05875ef1";
const DEPLOYMENT: &str = "QmXZiV6S13ha6QXq4dmaM3TB4CHcDxBMvGexSNu9Kc28EH";
const ALLOC: &str = "0x1111111111111111111111111111111111111111";

fn key(k: &str) -> PrivateKeySigner {
    PrivateKeySigner::from_str(k).unwrap()
}

struct Fixed(HashSet<Address>);

impl Operators for Fixed {
    fn is_operator(&self, _: Address, signer: Address) -> BoxFuture<'_, Result<bool, String>> {
        let ok = self.0.contains(&signer);
        Box::pin(async move { Ok(ok) })
    }
}

fn auth() -> Auth {
    Auth::new(
        key(INDEXER_KEY).address(),
        ORIGIN.into(),
        300,
        3600,
        [7u8; 32],
        Box::new(Fixed(HashSet::from([key(OPERATOR_KEY).address()]))),
    )
}

async fn harness() -> (MockServer, axum::Router) {
    let agent = MockServer::start().await;
    let state = Arc::new(AppState {
        auth: auth(),
        agent: Agent::connect(
            &agent.uri(),
            Some("agent-secret".into()),
            "eip155:42161".into(),
        )
        .await
        .unwrap(),
        allowed_origin: ORIGIN.into(),
        secure_cookie: true,
    });
    (agent, app(state))
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Option<String>, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        cookie,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn post(path: &str, body: Value, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, ORIGIN);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::get(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::empty()).unwrap()
}

/// Signs in with `k` and returns the `name=value` part of the session cookie.
async fn sign_in(app: &axum::Router, k: &PrivateKeySigner) -> (StatusCode, Option<String>) {
    let (_, _, ch) = send(
        app,
        post("/auth/challenge", json!({ "address": k.address() }), None),
    )
    .await;
    let sig = k
        .sign_message_sync(ch["message"].as_str().unwrap().as_bytes())
        .unwrap();
    let (status, cookie, _) = send(
        app,
        post(
            "/auth/verify",
            json!({ "nonce": ch["nonce"], "signature": sig.to_string() }),
            None,
        ),
    )
    .await;
    (
        status,
        cookie.map(|c| c.split(';').next().unwrap().to_string()),
    )
}

#[tokio::test]
async fn the_indexer_key_signs_in_and_gets_a_strict_http_only_cookie() {
    let (_agent, app) = harness().await;
    let k = key(INDEXER_KEY);
    let (_, _, ch) = send(
        &app,
        post("/auth/challenge", json!({ "address": k.address() }), None),
    )
    .await;
    let message = ch["message"].as_str().unwrap();
    assert!(message.contains(&k.address().to_string()));
    let sig = k.sign_message_sync(message.as_bytes()).unwrap();
    let (status, cookie, body) = send(
        &app,
        post(
            "/auth/verify",
            json!({ "nonce": ch["nonce"], "signature": sig.to_string() }),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["signer"], json!(k.address()));
    let cookie = cookie.unwrap();
    for attr in [
        "HttpOnly",
        "SameSite=Strict",
        "Secure",
        "Max-Age=3600",
        "Path=/",
    ] {
        assert!(cookie.contains(attr), "{cookie} lacks {attr}");
    }
    let pair = cookie.split(';').next().unwrap();
    let (status, _, s) = send(&app, get("/auth/session", Some(pair))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(s["indexer"], json!(k.address()));
}

#[tokio::test]
async fn an_on_chain_operator_signs_in() {
    let (_agent, app) = harness().await;
    let (status, cookie) = sign_in(&app, &key(OPERATOR_KEY)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(cookie.is_some());
}

#[tokio::test]
async fn a_signer_that_is_not_an_operator_is_refused() {
    let (_agent, app) = harness().await;
    let (status, cookie) = sign_in(&app, &key(STRANGER_KEY)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(cookie.is_none());
}

#[tokio::test]
async fn a_replayed_nonce_is_refused() {
    let a = auth();
    let k = key(INDEXER_KEY);
    let ch = a.challenge(k.address(), 1_000).unwrap();
    let sig = k
        .sign_message_sync(ch.message.as_bytes())
        .unwrap()
        .to_string();
    assert!(a.verify(&ch.nonce, &sig, 1_010).await.is_ok());
    assert_eq!(
        a.verify(&ch.nonce, &sig, 1_011).await,
        Err(AuthError::UnknownNonce)
    );
}

#[tokio::test]
async fn an_expired_nonce_is_refused_and_spent() {
    let a = auth();
    let k = key(INDEXER_KEY);
    let ch = a.challenge(k.address(), 1_000).unwrap();
    let sig = k
        .sign_message_sync(ch.message.as_bytes())
        .unwrap()
        .to_string();
    assert_eq!(
        a.verify(&ch.nonce, &sig, 1_300).await,
        Err(AuthError::Expired)
    );
    assert_eq!(
        a.verify(&ch.nonce, &sig, 1_010).await,
        Err(AuthError::UnknownNonce)
    );
}

#[tokio::test]
async fn a_signature_by_another_key_than_the_challenge_named_is_refused() {
    let a = auth();
    let ch = a.challenge(key(INDEXER_KEY).address(), 1_000).unwrap();
    let sig = key(OPERATOR_KEY)
        .sign_message_sync(ch.message.as_bytes())
        .unwrap()
        .to_string();
    assert_eq!(
        a.verify(&ch.nonce, &sig, 1_010).await,
        Err(AuthError::WrongSigner)
    );
    let ch = a.challenge(key(INDEXER_KEY).address(), 1_000).unwrap();
    assert_eq!(
        a.verify(&ch.nonce, "0xdeadbeef", 1_010).await,
        Err(AuthError::BadSignature)
    );
}

#[tokio::test]
async fn a_tampered_or_expired_session_is_refused() {
    let a = auth();
    let k = key(INDEXER_KEY);
    let ch = a.challenge(k.address(), 1_000).unwrap();
    let sig = k
        .sign_message_sync(ch.message.as_bytes())
        .unwrap()
        .to_string();
    let s = a.verify(&ch.nonce, &sig, 1_010).await.unwrap();
    let token = a.seal(&s);
    assert!(a.open(&token, 1_020).is_some());
    assert!(a.open(&token, s.expires_at).is_none());
    let (payload, tag) = token.split_once('.').unwrap();
    let forged = format!("{}x.{tag}", payload);
    assert!(a.open(&forged, 1_020).is_none());
}

#[tokio::test]
async fn nothing_reaches_the_agent_without_a_session() {
    let (agent, app) = harness().await;
    let (status, _, _) = send(&app, get("/actions", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send(&app, post("/actions/approve", json!({ "ids": [1] }), None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(agent.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_path_outside_the_allowlist_is_refused_without_touching_the_agent() {
    let (agent, app) = harness().await;
    let (_, cookie) = sign_in(&app, &key(INDEXER_KEY)).await;
    let cookie = cookie.unwrap();
    for path in [
        "/actions/execute",
        "/actions/delete",
        "/api/graphql",
        "/api/actions",
        "/cost-models",
        "/graphql",
        "/",
    ] {
        let (status, _, _) = send(
            &app,
            post(
                path,
                json!({ "query": "mutation { executeApprovedActions { id } }" }),
                Some(&cookie),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    // An action type outside the three is refused before the agent is asked.
    let (status, _, _) = send(
        &app,
        post(
            "/actions",
            json!({ "actions": [{ "type": "presentPOI", "deploymentID": DEPLOYMENT, "allocationID": ALLOC }] }),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(agent.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_queue_goes_to_the_agent_as_variables_with_the_token_attached() {
    let (agent, app) = harness().await;
    Mock::given(method("POST"))
        .and(has_header("authorization", "Bearer agent-secret"))
        .and(body_partial_json(json!({
            "variables": { "actions": [{
                "type": "unallocate",
                "deploymentID": DEPLOYMENT,
                "allocationID": ALLOC,
                "status": "queued",
                "source": "lodestar-cockpit",
                "protocolNetwork": "eip155:42161",
            }] }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "queueActions": [{ "id": 42, "type": "unallocate", "status": "queued" }] }
        })))
        .expect(1)
        .mount(&agent)
        .await;

    let (_, cookie) = sign_in(&app, &key(INDEXER_KEY)).await;
    let (status, _, body) = send(
        &app,
        post(
            "/actions",
            json!({ "actions": [{ "type": "unallocate", "deploymentID": DEPLOYMENT, "allocationID": ALLOC }] }),
            cookie.as_deref(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["actions"][0]["id"], json!(42));
    let sent = agent.received_requests().await.unwrap();
    let q: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert!(
        q["query"]
            .as_str()
            .unwrap()
            .starts_with("mutation queueActions(")
    );
    assert!(!body.to_string().contains("agent-secret"));
}

#[tokio::test]
async fn approve_cancel_list_and_rules_each_send_their_one_operation() {
    let (agent, app) = harness().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
        .mount(&agent)
        .await;
    let (_, cookie) = sign_in(&app, &key(INDEXER_KEY)).await;
    let c = cookie.as_deref();
    assert_eq!(
        send(&app, post("/actions/approve", json!({ "ids": [3, 4] }), c))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, post("/actions/cancel", json!({ "ids": [5] }), c))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, get("/actions?status=queued", c)).await.0,
        StatusCode::OK
    );
    assert_eq!(send(&app, get("/rules", c)).await.0, StatusCode::OK);
    assert_eq!(
        send(&app, get("/actions?status=everything", c)).await.0,
        StatusCode::BAD_REQUEST
    );

    let ops: Vec<String> = agent
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            v["query"]
                .as_str()
                .unwrap()
                .split('(')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        ops,
        [
            "mutation approveActions",
            "mutation cancelActions",
            "query actions",
            "query indexingRules"
        ]
    );
}

#[tokio::test]
async fn a_write_from_another_origin_is_refused() {
    let (agent, app) = harness().await;
    let (_, cookie) = sign_in(&app, &key(INDEXER_KEY)).await;
    let req = Request::post("/actions/approve")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::COOKIE, cookie.unwrap())
        .body(Body::from(json!({ "ids": [1] }).to_string()))
        .unwrap();
    assert_eq!(send(&app, req).await.0, StatusCode::FORBIDDEN);
    assert!(agent.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_agent_url_passes_the_ssrf_guard_or_the_cockpit_does_not_start() {
    for bad in [
        "http://169.254.169.254/latest/meta-data/",
        "http://[::ffff:169.254.169.254]/",
        "http://0.0.0.0:18000/",
        "file:///etc/passwd",
        "ftp://10.0.0.5/",
    ] {
        assert!(
            Agent::connect(bad, None, "eip155:42161".into())
                .await
                .is_err(),
            "{bad}"
        );
    }
    for ok in [
        "http://127.0.0.1:18000/",
        "http://10.0.0.5:18000/",
        "http://localhost:18000/",
    ] {
        assert!(
            Agent::connect(ok, None, "eip155:42161".into())
                .await
                .is_ok(),
            "{ok}"
        );
    }
}

#[tokio::test]
async fn operators_are_read_from_horizon_staking_is_authorized() {
    let rpc = MockServer::start().await;
    let selector = &keccak256("isAuthorized(address,address,address)")[..4];
    let expected = format!("0x{}", hex::encode(selector));
    Mock::given(method("POST"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let call = &body["params"][0];
            let data = call["input"]
                .as_str()
                .or(call["data"].as_str())
                .unwrap_or("");
            assert!(data.starts_with(&expected), "{data}");
            assert_eq!(
                call["to"].as_str().unwrap().to_lowercase(),
                kittiwake_chain::HORIZON_STAKING
            );
            // The operator argument is the third word; answer true only for the operator key.
            let operator = &data[10 + 64 * 2 + 24..10 + 64 * 3];
            let yes = operator == hex::encode(key(OPERATOR_KEY).address());
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": body["id"],
                "result": format!("0x{:064x}", yes as u8),
            }))
        })
        .mount(&rpc)
        .await;
    let ops = ChainOperators(kittiwake_chain::Chain::new(rpc.uri(), None));
    let indexer = key(INDEXER_KEY).address();
    assert!(
        ops.is_operator(indexer, key(OPERATOR_KEY).address())
            .await
            .unwrap()
    );
    assert!(
        !ops.is_operator(indexer, key(STRANGER_KEY).address())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn cors_admits_only_the_lodestar_origin_with_credentials() {
    let (_agent, app) = harness().await;
    let preflight = |origin: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/actions")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
            .body(Body::empty())
            .unwrap()
    };
    let ok = app.clone().oneshot(preflight(ORIGIN)).await.unwrap();
    assert_eq!(ok.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], ORIGIN);
    assert_eq!(
        ok.headers()[header::ACCESS_CONTROL_ALLOW_CREDENTIALS],
        "true"
    );
    let evil = app
        .clone()
        .oneshot(preflight("https://evil.example"))
        .await
        .unwrap();
    // A fixed origin is echoed as itself, never as the caller's, so the browser refuses.
    assert_eq!(evil.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], ORIGIN);
}

#[tokio::test]
async fn a_rule_goes_to_the_agent_in_wei_and_a_bad_one_never_leaves() {
    let (agent, app) = harness().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({
            "variables": { "rule": {
                "identifier": "global",
                "identifierType": "group",
                "decisionBasis": "rules",
                "minSignal": "500000000000000000000",
                "protocolNetwork": "eip155:42161",
            } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "setIndexingRule": { "identifier": "global", "decisionBasis": "rules" } }
        })))
        .expect(1)
        .mount(&agent)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({
            "variables": { "deployments": [{ "identifier": "global", "protocolNetwork": "eip155:42161" }] }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "deleteIndexingRules": true }
        })))
        .expect(1)
        .mount(&agent)
        .await;

    let (_, cookie) = sign_in(&app, &key(INDEXER_KEY)).await;
    let (status, _, body) = send(
        &app,
        post(
            "/rules",
            json!({ "identifier": "global", "decisionBasis": "rules", "minSignal": "500" }),
            cookie.as_deref(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rule"]["decisionBasis"], "rules");

    let (status, _, _) = send(
        &app,
        post(
            "/rules",
            json!({ "identifier": "all", "decisionBasis": "always" }),
            cookie.as_deref(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _, body) = send(
        &app,
        post(
            "/rules/delete",
            json!({ "identifier": "global" }),
            cookie.as_deref(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], json!(true));

    let sent = agent.received_requests().await.unwrap();
    assert_eq!(sent.len(), 2, "the refused rule must not reach the agent");
    let q: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert!(
        q["query"]
            .as_str()
            .unwrap()
            .starts_with("mutation setIndexingRule(")
    );
}

#[tokio::test]
async fn a_rule_write_needs_a_session_and_the_lodestar_origin() {
    let (agent, app) = harness().await;
    let (status, _, _) = send(
        &app,
        post("/rules", json!({ "identifier": "global" }), None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (_, cookie) = sign_in(&app, &key(INDEXER_KEY)).await;
    let req = Request::post("/rules/delete")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::COOKIE, cookie.unwrap())
        .body(Body::from(json!({ "identifier": "global" }).to_string()))
        .unwrap();
    let (status, _, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(agent.received_requests().await.unwrap().is_empty());
}
