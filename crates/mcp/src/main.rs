//! The Lodestar MCP server, over stdio.
//!
//! It deliberately proxies the public API rather than re-implementing handlers. There is one
//! source of truth for rate limits, provenance and route policy, and an MCP client is simply
//! another consumer of it. Tools are generated from the OpenAPI document, so a route withheld
//! there - `/api/sql/query` above all - cannot reach a model through this door either. #37

use std::io::{self, BufRead, Write};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde_json::{Value, json};

const DEFAULT_ORIGIN: &str = "https://api.lodestar-dashboard.com";

fn document() -> &'static Value {
    static DOCUMENT: OnceLock<Value> = OnceLock::new();
    DOCUMENT.get_or_init(|| kittiwake_edge::openapi::document(env!("CARGO_PKG_VERSION")))
}

fn paths() -> impl Iterator<Item = &'static String> {
    document()["paths"]
        .as_object()
        .into_iter()
        .flat_map(|paths| paths.keys())
}

fn tools() -> Vec<Value> {
    paths()
        .filter_map(|path| {
            let summary = document()["paths"][path].get("get")?.get("summary")?;
            Some(json!({
                "name": name_for(path),
                "description": summary,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "object", "description": "Values for {path} parameters." },
                        "query": { "type": "object", "description": "Optional URL query parameters." }
                    },
                    "additionalProperties": false
                }
            }))
        })
        .collect()
}

/// A tool name for a path. Separators collapse, so `/api/indexer/{address}` is
/// `lodestar_indexer_address` rather than carrying the brace's underscore as well.
fn name_for(path: &str) -> String {
    let words = path
        .trim_start_matches("/api/")
        .split(['/', '{', '}', '-'])
        .filter(|word| !word.is_empty());
    std::iter::once("lodestar")
        .chain(words)
        .collect::<Vec<_>>()
        .join("_")
}

fn path_for(name: &str) -> Option<&'static String> {
    paths().find(|path| name_for(path) == name)
}

/// Turn a tool call into the URL it proxies to.
///
/// A path parameter left unsubstituted would otherwise be requested literally, and `/api/indexer/
/// %7Baddress%7D` is a 404 dressed up as an answer, so a missing one is refused here instead.
fn url_for(origin: &str, name: &str, arguments: &Value) -> Result<String, String> {
    let template = path_for(name).ok_or("tools/call requires a known Lodestar tool name")?;
    let mut path = template.clone();
    if let Some(values) = arguments.get("path").and_then(Value::as_object) {
        for (key, value) in values {
            if let Some(value) = value.as_str() {
                path = path.replace(&format!("{{{key}}}"), &urlencoding::encode(value));
            }
        }
    }
    if path.contains('{') {
        return Err(format!(
            "{template} needs a path parameter that was not given"
        ));
    }
    let mut url = format!("{origin}{path}");
    let pairs: Vec<String> = arguments
        .get("query")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| {
            let value = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some(format!(
                "{}={}",
                urlencoding::encode(key),
                urlencoding::encode(&value)
            ))
        })
        .collect();
    if !pairs.is_empty() {
        url.push('?');
        url.push_str(&pairs.join("&"));
    }
    Ok(url)
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// What one line of input asks for, decided without touching the network.
#[derive(Debug, PartialEq)]
enum Action {
    Respond(Value),
    Fetch {
        id: Value,
        url: String,
    },
    /// A notification. JSON-RPC forbids a response to one, and `notifications/initialized` arrives
    /// from every client immediately after the handshake.
    Silent,
}

fn plan(line: &str, origin: &str) -> Action {
    let request: Value = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(error) => {
            return Action::Respond(failure(
                Value::Null,
                -32700,
                format!("invalid JSON: {error}"),
            ));
        }
    };
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Action::Respond(failure(Value::Null, -32600, "request has no method"));
    };
    let Some(id) = request.get("id").cloned() else {
        return Action::Silent;
    };

    match method {
        "initialize" => Action::Respond(reply(
            id,
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "kittiwake", "version": env!("CARGO_PKG_VERSION") }
            }),
        )),
        "tools/list" => Action::Respond(reply(id, json!({ "tools": tools() }))),
        "tools/call" => {
            let params = &request["params"];
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match url_for(
                origin,
                name,
                params.get("arguments").unwrap_or(&Value::Null),
            ) {
                Ok(url) => Action::Fetch { id, url },
                Err(why) => Action::Respond(failure(id, -32602, why)),
            }
        }
        _ => Action::Respond(failure(id, -32601, "method not found")),
    }
}

fn fetched(id: Value, ok: bool, body: String) -> Value {
    reply(
        id,
        json!({ "content": [{ "type": "text", "text": body }], "isError": !ok }),
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let origin = std::env::var("KITTIWAKE_API_URL")
        .unwrap_or_else(|_| DEFAULT_ORIGIN.into())
        .trim_end_matches('/')
        .to_string();
    let client = reqwest::Client::builder()
        .user_agent(concat!("kittiwake-mcp/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("construct MCP HTTP client")?;

    for line in io::stdin().lock().lines() {
        let response = match plan(&line.context("read MCP input")?, &origin) {
            Action::Silent => continue,
            Action::Respond(response) => response,
            Action::Fetch { id, url } => match client.get(&url).send().await {
                Ok(response) => {
                    let ok = response.status().is_success();
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|e| format!("unreadable response: {e}"));
                    fetched(id, ok, body)
                }
                Err(error) => failure(id, -32000, format!("Lodestar API unavailable: {error}")),
            },
        };
        println!("{response}");
        io::stdout().flush().context("flush MCP output")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "https://api.example.test";

    fn call(name: &str, arguments: Value) -> Action {
        plan(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":name,"arguments":arguments}})
            .to_string(),
            ORIGIN,
        )
    }

    /// The load-bearing property. Two paths that collapse to one tool name would make one of them
    /// unreachable, and `path_for` would silently answer with whichever came first.
    #[test]
    fn every_published_path_has_a_name_of_its_own() {
        let mut seen = std::collections::BTreeMap::new();
        for path in paths() {
            let name = name_for(path);
            if let Some(other) = seen.insert(name.clone(), path) {
                panic!("{path} and {other} both become the tool {name}");
            }
            assert_eq!(
                path_for(&name),
                Some(path),
                "{name} does not round-trip to {path}"
            );
        }
        assert!(seen.len() > 25, "only {} tools generated", seen.len());
    }

    /// The whole point of deriving tools from the document rather than from the router. Anything
    /// `WITHHELD` there is unreachable here without a second decision being made.
    #[test]
    fn a_withheld_route_is_not_a_tool() {
        for (path, _) in kittiwake_edge::openapi::WITHHELD {
            assert_eq!(
                path_for(&name_for(path)),
                None,
                "{path} is withheld and still reachable as a tool"
            );
        }
        assert!(
            path_for("lodestar_sql_named").is_some(),
            "the named tier is published and must be callable"
        );
    }

    #[test]
    fn a_path_parameter_is_substituted_and_encoded() {
        let url = url_for(
            ORIGIN,
            "lodestar_indexer_address",
            &json!({"path": {"address": "0xAb/Cd"}}),
        )
        .unwrap();
        assert_eq!(url, format!("{ORIGIN}/api/indexer/0xAb%2FCd"));
    }

    /// Requesting the template verbatim would return a 404 that reads like an empty answer.
    #[test]
    fn a_missing_path_parameter_is_refused_rather_than_requested() {
        let err = url_for(ORIGIN, "lodestar_indexer_address", &json!({})).unwrap_err();
        assert!(err.contains("path parameter"), "{err}");
        assert!(matches!(
            call("lodestar_indexer_address", json!({})),
            Action::Respond(_)
        ));
    }

    #[test]
    fn query_arguments_are_encoded_and_numbers_survive_as_themselves() {
        let url = url_for(
            ORIGIN,
            "lodestar_epochs",
            &json!({"query": {"count": 5, "q": "a b"}}),
        )
        .unwrap();
        assert_eq!(url, format!("{ORIGIN}/api/epochs?count=5&q=a%20b"));
    }

    #[test]
    fn an_unknown_tool_is_an_error_and_not_a_request() {
        assert!(matches!(
            call("lodestar_not_a_route", json!({})),
            Action::Respond(_)
        ));
    }

    /// A response to a notification is a protocol violation, and `notifications/initialized`
    /// arrives from every client. Answering it once broke the handshake.
    #[test]
    fn a_notification_is_answered_with_silence() {
        for line in [
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","method":"tools/list"}"#,
        ] {
            assert_eq!(plan(line, ORIGIN), Action::Silent, "{line}");
        }
    }

    #[test]
    fn a_request_is_answered_even_when_it_is_nonsense() {
        let cases = [
            (r#"not json at all"#, -32700),
            (r#"{"jsonrpc":"2.0","id":1}"#, -32600),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#,
                -32601,
            ),
        ];
        for (line, code) in cases {
            let Action::Respond(response) = plan(line, ORIGIN) else {
                panic!("{line} should have been answered");
            };
            assert_eq!(response["error"]["code"], code, "{line}");
        }
    }

    #[test]
    fn initialize_and_tools_list_answer_with_the_published_surface() {
        let Action::Respond(response) =
            plan(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, ORIGIN)
        else {
            panic!("tools/list should have been answered");
        };
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), paths().count());
        assert!(tools.iter().all(|t| t["description"].is_string()));

        let Action::Respond(response) =
            plan(r#"{"jsonrpc":"2.0","id":0,"method":"initialize"}"#, ORIGIN)
        else {
            panic!("initialize should have been answered");
        };
        assert_eq!(response["result"]["serverInfo"]["name"], "kittiwake");
    }
}
