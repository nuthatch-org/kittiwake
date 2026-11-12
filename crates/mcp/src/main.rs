//! The Lodestar MCP server, over stdio.
//!
//! It deliberately proxies the public API rather than re-implementing handlers. There is one
//! source of truth for rate limits, provenance and route policy, and an MCP client is simply
//! another consumer of it. Tools are generated from the OpenAPI document, so a route withheld
//! there - `/api/sql/query` above all - cannot reach a model through this door either. #37

use std::io::{self, BufRead, Write};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

const DEFAULT_ORIGIN: &str = "https://api.lodestar-dashboard.com";

/// The stdio loop answers one call at a time, so a hung upstream would stall every call behind it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn document() -> &'static Value {
    static DOCUMENT: OnceLock<Value> = OnceLock::new();
    DOCUMENT.get_or_init(|| kittiwake_edge::openapi::document(env!("CARGO_PKG_VERSION")))
}

/// One published operation: a path, the method the router binds, and its description.
struct Tool {
    path: &'static String,
    method: &'static str,
    operation: &'static Value,
}

fn operations() -> impl Iterator<Item = Tool> {
    document()["paths"]
        .as_object()
        .into_iter()
        .flat_map(|paths| paths.iter())
        .flat_map(|(path, item)| {
            item.as_object().into_iter().flat_map(move |methods| {
                methods.iter().map(move |(method, operation)| Tool {
                    path,
                    method: method.as_str(),
                    operation,
                })
            })
        })
}

fn tools() -> Vec<Value> {
    operations()
        .map(|tool| {
            json!({
                "name": name_for(tool.path, tool.method),
                "description": tool.operation["summary"],
                "inputSchema": input_schema(&tool),
            })
        })
        .collect()
}

/// What a model needs to fill in to make the call: the path values, the query parameters this
/// route reads, and for a POST the body.
fn input_schema(tool: &Tool) -> Value {
    let params = tool.operation["parameters"].as_array();
    let named = |place: &str| -> (Map<String, Value>, Vec<Value>) {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for p in params.into_iter().flatten().filter(|p| p["in"] == place) {
            let name = p["name"].as_str().unwrap_or_default();
            let mut schema = p["schema"].clone();
            if let Some(doc) = p["description"].as_str().filter(|d| !d.is_empty()) {
                schema["description"] = doc.into();
            }
            properties.insert(name.to_string(), schema);
            if p["required"] == true {
                required.push(name.into());
            }
        }
        (properties, required)
    };

    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();

    let (path, path_required) = named("path");
    if !path.is_empty() {
        properties.insert(
            "path".into(),
            json!({ "type": "object", "properties": path, "required": path_required,
                    "additionalProperties": false }),
        );
        required.push("path".into());
    }

    let (query, query_required) = named("query");
    let free_form = tool.operation["x-query-free-form"] == true;
    if !query.is_empty() || free_form {
        let mut schema = json!({ "type": "object", "properties": query,
                                 "required": query_required,
                                 "additionalProperties": free_form });
        if free_form {
            schema["description"] = "Further URL query parameters this route reads.".into();
        }
        properties.insert("query".into(), schema);
        if !query_required.is_empty() {
            required.push("query".into());
        }
    }

    if let Some(body) = tool.operation["requestBody"]["content"]["application/json"].get("schema") {
        properties.insert("body".into(), body.clone());
        required.push("body".into());
    }

    json!({ "type": "object", "properties": properties, "required": required,
            "additionalProperties": false })
}

/// A tool name for a path. Separators collapse, so `/api/indexer/{address}` is
/// `lodestar_indexer_address` rather than carrying the brace's underscore as well. A method other
/// than GET is appended, so a path served two ways has two tools.
fn name_for(path: &str, method: &str) -> String {
    let words = path
        .trim_start_matches("/api/")
        .split(['/', '{', '}', '-'])
        .filter(|word| !word.is_empty());
    let name = std::iter::once("lodestar")
        .chain(words)
        .collect::<Vec<_>>()
        .join("_");
    if method == "get" {
        name
    } else {
        format!("{name}_{method}")
    }
}

fn tool_for(name: &str) -> Option<Tool> {
    operations().find(|tool| name_for(tool.path, tool.method) == name)
}

/// What a tool call becomes on the wire.
#[derive(Debug, PartialEq)]
struct Call {
    method: &'static str,
    url: String,
    body: Option<Value>,
}

/// Turn a tool call into the request it proxies to.
///
/// A path parameter left unsubstituted would otherwise be requested literally, and `/api/indexer/
/// %7Baddress%7D` is a 404 dressed up as an answer, so a missing one is refused here instead.
fn call_for(origin: &str, name: &str, arguments: &Value) -> Result<Call, String> {
    let tool = tool_for(name).ok_or("tools/call requires a known Lodestar tool name")?;
    let template = tool.path;
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
    let method = match tool.method {
        "get" => "GET",
        "post" => "POST",
        other => return Err(format!("{other} is not a method this server sends")),
    };
    let body = (method != "GET").then(|| arguments.get("body").cloned().unwrap_or(json!({})));
    Ok(Call { method, url, body })
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
        call: Call,
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
            match call_for(
                origin,
                name,
                params.get("arguments").unwrap_or(&Value::Null),
            ) {
                Ok(call) => Action::Fetch { id, call },
                Err(why) => Action::Respond(failure(id, -32602, why)),
            }
        }
        _ => Action::Respond(failure(id, -32601, "method not found")),
    }
}

/// A non-2xx answer leads with its status: a 405 or a bare 404 has an empty body, and a model given
/// only that has nothing to act on.
fn fetched(id: Value, status: reqwest::StatusCode, body: String) -> Value {
    let text = if status.is_success() {
        body
    } else if body.trim().is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {body}")
    };
    reply(
        id,
        json!({ "content": [{ "type": "text", "text": text }], "isError": !status.is_success() }),
    )
}

/// Send one call. POST bodies go as JSON.
async fn send(client: &reqwest::Client, call: Call) -> reqwest::Result<reqwest::Response> {
    match call.body {
        Some(body) => client.post(&call.url).json(&body).send().await,
        None => client.get(&call.url).send().await,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let origin = std::env::var("KITTIWAKE_API_URL")
        .unwrap_or_else(|_| DEFAULT_ORIGIN.into())
        .trim_end_matches('/')
        .to_string();
    let client = reqwest::Client::builder()
        .user_agent(concat!("kittiwake-mcp/", env!("CARGO_PKG_VERSION")))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("construct MCP HTTP client")?;

    for line in io::stdin().lock().lines() {
        let response = match plan(&line.context("read MCP input")?, &origin) {
            Action::Silent => continue,
            Action::Respond(response) => response,
            Action::Fetch { id, call } => match send(&client, call).await {
                Ok(response) => {
                    let status = response.status();
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|e| format!("unreadable response: {e}"));
                    fetched(id, status, body)
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
        for tool in operations() {
            let name = name_for(tool.path, tool.method);
            if let Some(other) = seen.insert(name.clone(), tool.path) {
                panic!("{} and {other} both become the tool {name}", tool.path);
            }
            assert_eq!(
                tool_for(&name).map(|t| (t.path, t.method)),
                Some((tool.path, tool.method)),
                "{name} does not round-trip"
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
                operations()
                    .find(|t| t.path == path)
                    .map(|t| t.path.as_str()),
                None,
                "{path} is withheld and still reachable as a tool"
            );
        }
        assert!(
            tool_for("lodestar_sql_named_post").is_some(),
            "the named tier is published and must be callable"
        );
    }

    #[test]
    fn a_path_parameter_is_substituted_and_encoded() {
        let call = call_for(
            ORIGIN,
            "lodestar_indexer_address",
            &json!({"path": {"address": "0xAb/Cd"}}),
        )
        .unwrap();
        assert_eq!(call.url, format!("{ORIGIN}/api/indexer/0xAb%2FCd"));
    }

    /// Requesting the template verbatim would return a 404 that reads like an empty answer.
    #[test]
    fn a_missing_path_parameter_is_refused_rather_than_requested() {
        let err = call_for(ORIGIN, "lodestar_indexer_address", &json!({})).unwrap_err();
        assert!(err.contains("path parameter"), "{err}");
        assert!(matches!(
            call("lodestar_indexer_address", json!({})),
            Action::Respond(_)
        ));
    }

    #[test]
    fn query_arguments_are_encoded_and_numbers_survive_as_themselves() {
        let call = call_for(
            ORIGIN,
            "lodestar_epochs",
            &json!({"query": {"count": 5, "q": "a b"}}),
        )
        .unwrap();
        assert_eq!(call.url, format!("{ORIGIN}/api/epochs?count=5&q=a%20b"));
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
        assert_eq!(tools.len(), operations().count());
        assert!(tools.iter().all(|t| t["description"].is_string()));

        let Action::Respond(response) =
            plan(r#"{"jsonrpc":"2.0","id":0,"method":"initialize"}"#, ORIGIN)
        else {
            panic!("initialize should have been answered");
        };
        assert_eq!(response["result"]["serverInfo"]["name"], "kittiwake");
    }

    #[test]
    fn a_post_only_route_is_sent_as_a_post_with_its_body() {
        let call = call_for(
            ORIGIN,
            "lodestar_sql_named_post",
            &json!({"body": {"name": "x", "args": {}}}),
        )
        .unwrap();
        assert_eq!(call.method, "POST");
        assert_eq!(call.url, format!("{ORIGIN}/api/sql/named"));
        assert_eq!(call.body, Some(json!({"name": "x", "args": {}})));

        let call = call_for(ORIGIN, "lodestar_subgraph_names_post", &json!({})).unwrap();
        assert_eq!(call.method, "POST");
        assert!(
            tool_for("lodestar_subgraph_names").is_none(),
            "it has no GET"
        );
        assert_eq!(
            call_for(ORIGIN, "lodestar_sql_named", &json!({}))
                .unwrap()
                .method,
            "GET"
        );
    }

    #[test]
    fn a_tool_declares_the_parameters_its_route_reads() {
        let Action::Respond(response) =
            plan(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, ORIGIN)
        else {
            panic!("tools/list should have been answered");
        };
        let tools = response["result"]["tools"].as_array().unwrap();
        let schema =
            |name: &str| tools.iter().find(|t| t["name"] == name).unwrap()["inputSchema"].clone();

        let portfolio = schema("lodestar_portfolio");
        assert_eq!(
            portfolio["properties"]["query"]["properties"]["address"]["type"],
            "string"
        );
        assert_eq!(
            portfolio["properties"]["query"]["additionalProperties"],
            false
        );

        let indexer = schema("lodestar_indexer_address");
        assert_eq!(indexer["required"], json!(["path"]));
        assert_eq!(
            indexer["properties"]["path"]["required"],
            json!(["address"])
        );

        let named = schema("lodestar_sql_named_post");
        assert_eq!(named["required"], json!(["body"]));
        assert_eq!(
            named["properties"]["body"]["properties"]["name"]["type"],
            "string"
        );
    }

    #[test]
    fn an_error_answer_leads_with_its_status_even_when_the_body_is_empty() {
        let text = |r: Value| {
            r["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let empty = fetched(
            json!(1),
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
            String::new(),
        );
        assert_eq!(text(empty.clone()), "HTTP 405 Method Not Allowed");
        assert_eq!(empty["result"]["isError"], true);
        let with_body = fetched(json!(1), reqwest::StatusCode::BAD_REQUEST, "bad".into());
        assert_eq!(text(with_body), "HTTP 400 Bad Request: bad");
        let ok = fetched(json!(1), reqwest::StatusCode::OK, "{}".into());
        assert_eq!(text(ok.clone()), "{}");
        assert_eq!(ok["result"]["isError"], false);
    }
}
