//! Substreams, spoken over the Connect protocol rather than gRPC: one POST whose request and
//! response bodies are length-prefixed protobuf frames, so plain reqwest will do and no gRPC stack
//! or protoc step comes into the build.
//!
//! Only `ethereum-common`'s `filtered_events` is used, vendored at a pinned version. The messages
//! below declare just the fields read or sent; prost skips the rest.

use std::time::Duration;

use bytes::{Buf, BytesMut};
use kittiwake_types::ApiError;
use prost::Message;
use prost::encoding::{WireType, decode_key, decode_varint, encode_key, encode_varint};

const ETHEREUM_COMMON: &[u8] = include_bytes!("../substreams/ethereum-common-v0.4.0.spkg");
const MODULE: &str = "filtered_events";
const PATH: &str = "/sf.substreams.rpc.v2.Stream/Blocks";
const END_STREAM: u8 = 0x02;

#[derive(Clone, PartialEq, Message)]
struct Request {
    #[prost(int64, tag = "1")]
    start_block_num: i64,
    #[prost(uint64, tag = "3")]
    stop_block_num: u64,
    #[prost(bool, tag = "4")]
    final_blocks_only: bool,
    #[prost(bool, tag = "5")]
    production_mode: bool,
    #[prost(string, tag = "6")]
    output_module: String,
    /// An encoded `sf.substreams.v1.Modules`, carried as bytes so the package passes through
    /// without a model of every module kind.
    #[prost(bytes = "vec", tag = "7")]
    modules: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct Response {
    #[prost(message, optional, tag = "3")]
    block_scoped_data: Option<BlockScopedData>,
    #[prost(message, optional, tag = "5")]
    fatal_error: Option<FatalError>,
}

#[derive(Clone, PartialEq, Message)]
struct FatalError {
    #[prost(string, tag = "1")]
    module: String,
    #[prost(string, tag = "2")]
    reason: String,
}

#[derive(Clone, PartialEq, Message)]
struct BlockScopedData {
    #[prost(message, optional, tag = "1")]
    output: Option<MapModuleOutput>,
    #[prost(message, optional, tag = "2")]
    clock: Option<Clock>,
}

#[derive(Clone, PartialEq, Message)]
struct MapModuleOutput {
    #[prost(message, optional, tag = "2")]
    map_output: Option<Any>,
}

#[derive(Clone, PartialEq, Message)]
struct Any {
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct Clock {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(uint64, tag = "2")]
    number: u64,
    #[prost(message, optional, tag = "3")]
    timestamp: Option<Timestamp>,
}

#[derive(Clone, PartialEq, Message)]
struct Timestamp {
    #[prost(int64, tag = "1")]
    seconds: i64,
}

#[derive(Clone, PartialEq, Message)]
struct Events {
    #[prost(message, repeated, tag = "2")]
    events: Vec<Event>,
}

#[derive(Clone, PartialEq, Message)]
struct Event {
    #[prost(message, optional, tag = "1")]
    log: Option<Log>,
    #[prost(string, tag = "2")]
    tx_hash: String,
}

#[derive(Clone, PartialEq, Message)]
struct Log {
    #[prost(bytes = "vec", tag = "1")]
    address: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    topics: Vec<Vec<u8>>,
    #[prost(bytes = "vec", tag = "3")]
    data: Vec<u8>,
    /// `block_index` is the log's position in the block, which is what `eth_getLogs` calls
    /// `logIndex`. `index` (tag 4) is its position in the transaction, and is not that.
    #[prost(uint32, tag = "6")]
    block_index: u32,
}

/// One log as Substreams delivered it. Hashes are lowercase hex without `0x`.
#[derive(Debug, Clone, PartialEq)]
pub struct LoggedEvent {
    pub block: u64,
    pub block_hash: String,
    pub timestamp: i64,
    pub tx_hash: String,
    pub log_index: u32,
    pub address: Vec<u8>,
    pub topics: Vec<Vec<u8>>,
    pub data: Vec<u8>,
}

#[derive(Clone)]
pub struct Substreams {
    endpoint: String,
    api_key: String,
    http: reqwest::Client,
}

impl Substreams {
    pub fn new(endpoint: &str, api_key: &str) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            http: reqwest::Client::new(),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Every log matching `query` (ethereum-common's query syntax) in `[start, stop)`, final
    /// blocks only. An answer is returned only when the server closes the stream cleanly, so a
    /// stream cut short is an error and never a short list.
    pub async fn filtered_events(
        &self,
        query: &str,
        start: u64,
        stop: u64,
        timeout: Duration,
    ) -> Result<Vec<LoggedEvent>, ApiError> {
        let request = Request {
            start_block_num: i64::try_from(start)
                .map_err(|_| ApiError::BadRequest("start block out of range".into()))?,
            stop_block_num: stop,
            final_blocks_only: true,
            production_mode: true,
            output_module: MODULE.into(),
            modules: modules_with_params(ETHEREUM_COMMON, MODULE, query)
                .map_err(ApiError::Internal)?,
        };
        let mut response = self
            .http
            .post(format!("{}{PATH}", self.endpoint))
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("x-api-key", &self.api_key)
            .timeout(timeout)
            .body(frame(&request.encode_to_vec()))
            .send()
            .await
            .map_err(|e| unavailable(&format!("request failed: {}", e.without_url())))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(unavailable(&format!(
                "HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }

        let mut buf = BytesMut::new();
        let mut out = Vec::new();
        loop {
            while let Some((flags, message)) = next_frame(&mut buf) {
                if flags & END_STREAM != 0 {
                    return end_of_stream(&message).map(|()| out);
                }
                let r = Response::decode(message.as_slice())
                    .map_err(|e| unavailable(&format!("undecodable frame: {e}")))?;
                if let Some(err) = r.fatal_error {
                    return Err(unavailable(&format!("{}: {}", err.module, err.reason)));
                }
                if let Some(data) = r.block_scoped_data {
                    collect(data, &mut out)?;
                }
            }
            match response.chunk().await {
                Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
                Ok(None) => return Err(unavailable("stream ended without an end-of-stream frame")),
                Err(e) => return Err(unavailable(&format!("stream broke: {e}"))),
            }
        }
    }
}

fn unavailable(why: &str) -> ApiError {
    ApiError::Unavailable(format!("substreams: {why}"))
}

fn collect(data: BlockScopedData, out: &mut Vec<LoggedEvent>) -> Result<(), ApiError> {
    let Some(clock) = data.clock else {
        return Err(unavailable("block without a clock"));
    };
    let Some(any) = data.output.and_then(|o| o.map_output) else {
        return Ok(());
    };
    let events = Events::decode(any.value.as_slice())
        .map_err(|e| unavailable(&format!("undecodable events: {e}")))?;
    for event in events.events {
        let Some(log) = event.log else { continue };
        out.push(LoggedEvent {
            block: clock.number,
            block_hash: clock.id.trim_start_matches("0x").to_ascii_lowercase(),
            timestamp: clock.timestamp.as_ref().map_or(0, |t| t.seconds),
            tx_hash: event.tx_hash.trim_start_matches("0x").to_ascii_lowercase(),
            log_index: log.block_index,
            address: log.address,
            topics: log.topics,
            data: log.data,
        });
    }
    Ok(())
}

/// The closing frame is JSON: `{}` on success, `{"error": {...}}` otherwise.
fn end_of_stream(message: &[u8]) -> Result<(), ApiError> {
    let v: serde_json::Value = serde_json::from_slice(message).unwrap_or_default();
    match v.get("error") {
        None => Ok(()),
        Some(e) => Err(unavailable(&format!(
            "{}: {}",
            e.get("code").and_then(|c| c.as_str()).unwrap_or("error"),
            e.get("message").and_then(|m| m.as_str()).unwrap_or("")
        ))),
    }
}

fn frame(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 5);
    out.push(0);
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

fn next_frame(buf: &mut BytesMut) -> Option<(u8, Vec<u8>)> {
    if buf.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if buf.len() < 5 + len {
        return None;
    }
    let flags = buf[0];
    buf.advance(5);
    Some((flags, buf.split_to(len).to_vec()))
}

enum Value<'a> {
    Varint(u64),
    Fixed(&'a [u8]),
    Len(&'a [u8]),
}

fn fields(mut b: &[u8]) -> Result<Vec<(u32, WireType, Value<'_>)>, String> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let (tag, wire) = decode_key(&mut b).map_err(|e| e.to_string())?;
        let value = match wire {
            WireType::Varint => Value::Varint(decode_varint(&mut b).map_err(|e| e.to_string())?),
            WireType::SixtyFourBit => Value::Fixed(take(&mut b, 8)?),
            WireType::ThirtyTwoBit => Value::Fixed(take(&mut b, 4)?),
            WireType::LengthDelimited => {
                let n = decode_varint(&mut b).map_err(|e| e.to_string())? as usize;
                Value::Len(take(&mut b, n)?)
            }
            WireType::StartGroup | WireType::EndGroup => {
                return Err("protobuf groups are not expected in a package".into());
            }
        };
        out.push((tag, wire, value));
    }
    Ok(out)
}

fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
    if b.len() < n {
        return Err("truncated protobuf field".into());
    }
    let (head, rest) = b.split_at(n);
    *b = rest;
    Ok(head)
}

fn emit(tag: u32, wire: WireType, value: &Value<'_>, out: &mut Vec<u8>) {
    encode_key(tag, wire, out);
    match value {
        Value::Varint(v) => encode_varint(*v, out),
        Value::Fixed(raw) => out.extend_from_slice(raw),
        Value::Len(raw) => {
            encode_varint(raw.len() as u64, out);
            out.extend_from_slice(raw);
        }
    }
}

/// The package's `Modules` with `module`'s params input set to `params`, which is what the CLI's
/// `--params module=…` does. Everything else is copied byte for byte.
///
/// Field numbers: Package.modules 6; Modules.modules 1; Module.name 1, Module.inputs 6;
/// Input.params 4; Params.value 1.
fn modules_with_params(package: &[u8], module: &str, params: &str) -> Result<Vec<u8>, String> {
    let modules = fields(package)?
        .into_iter()
        .find_map(|(tag, _, v)| match (tag, v) {
            (6, Value::Len(raw)) => Some(raw),
            _ => None,
        })
        .ok_or("package has no modules")?;

    let mut params_msg = Vec::new();
    emit(
        1,
        WireType::LengthDelimited,
        &Value::Len(params.as_bytes()),
        &mut params_msg,
    );

    let mut patched = 0;
    let mut out = Vec::with_capacity(modules.len() + params.len());
    for (tag, wire, value) in fields(modules)? {
        let Value::Len(raw) = value else {
            emit(tag, wire, &value, &mut out);
            continue;
        };
        let parts = fields(raw)?;
        let named = parts
            .iter()
            .any(|(t, _, v)| *t == 1 && matches!(v, Value::Len(n) if *n == module.as_bytes()));
        if tag != 1 || !named {
            emit(tag, wire, &Value::Len(raw), &mut out);
            continue;
        }
        let mut rebuilt = Vec::with_capacity(raw.len() + params.len());
        for (t, w, v) in &parts {
            let Value::Len(input) = v else {
                emit(*t, *w, v, &mut rebuilt);
                continue;
            };
            if *t == 6 && fields(input)?.iter().any(|(it, _, _)| *it == 4) {
                let mut new_input = Vec::new();
                emit(
                    4,
                    WireType::LengthDelimited,
                    &Value::Len(&params_msg),
                    &mut new_input,
                );
                emit(
                    6,
                    WireType::LengthDelimited,
                    &Value::Len(&new_input),
                    &mut rebuilt,
                );
                patched += 1;
            } else {
                emit(*t, *w, v, &mut rebuilt);
            }
        }
        emit(tag, wire, &Value::Len(&rebuilt), &mut out);
    }
    if patched != 1 {
        return Err(format!(
            "expected one params input on {module}, found {patched}"
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module_params(modules: &[u8], module: &str) -> Vec<String> {
        let mut found = Vec::new();
        for (tag, _, v) in fields(modules).unwrap() {
            let (1, Value::Len(raw)) = (tag, v) else {
                continue;
            };
            let parts = fields(raw).unwrap();
            if !parts
                .iter()
                .any(|(t, _, v)| *t == 1 && matches!(v, Value::Len(n) if *n == module.as_bytes()))
            {
                continue;
            }
            for (t, _, v) in parts {
                let (6, Value::Len(input)) = (t, v) else {
                    continue;
                };
                for (it, _, iv) in fields(input).unwrap() {
                    let (4, Value::Len(p)) = (it, iv) else {
                        continue;
                    };
                    for (pt, _, pv) in fields(p).unwrap() {
                        if let (1, Value::Len(s)) = (pt, pv) {
                            found.push(String::from_utf8(s.to_vec()).unwrap());
                        }
                    }
                }
            }
        }
        found
    }

    #[test]
    fn sets_the_params_of_the_named_module_only() {
        let original = fields(ETHEREUM_COMMON)
            .unwrap()
            .into_iter()
            .find_map(|(t, _, v)| match (t, v) {
                (6, Value::Len(raw)) => Some(raw.to_vec()),
                _ => None,
            })
            .unwrap();
        let patched = modules_with_params(ETHEREUM_COMMON, MODULE, "evt_addr:0xabc").unwrap();

        assert_eq!(module_params(&patched, MODULE), vec!["evt_addr:0xabc"]);
        assert_eq!(
            module_params(&patched, "filtered_calls"),
            module_params(&original, "filtered_calls")
        );
        // Only the params string changed size; the wasm and every other module came through whole.
        let before = module_params(&original, MODULE).concat().len();
        assert_eq!(
            patched.len() - "evt_addr:0xabc".len(),
            original.len() - before
        );
    }

    #[test]
    fn refuses_a_module_the_package_does_not_have() {
        let err = modules_with_params(ETHEREUM_COMMON, "no_such_module", "x").unwrap_err();
        assert!(err.contains("found 0"), "{err}");
    }

    #[test]
    fn frames_split_across_chunks_are_reassembled() {
        let a = frame(b"hello");
        let mut b = frame(b"{}");
        b[0] = END_STREAM;
        let all = [a.as_slice(), b.as_slice()].concat();

        let mut buf = BytesMut::from(&all[..3]);
        assert!(next_frame(&mut buf).is_none());
        buf.extend_from_slice(&all[3..]);
        assert_eq!(next_frame(&mut buf), Some((0, b"hello".to_vec())));
        assert_eq!(next_frame(&mut buf), Some((END_STREAM, b"{}".to_vec())));
        assert!(next_frame(&mut buf).is_none());
    }

    #[test]
    fn end_of_stream_reports_the_servers_error() {
        assert!(end_of_stream(b"{}").is_ok());
        let err = end_of_stream(br#"{"error":{"code":"unauthenticated","message":"bad key"}}"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("unauthenticated: bad key"),
            "{err}"
        );
    }

    #[test]
    fn events_take_the_block_level_log_index() {
        let events = Events {
            events: vec![Event {
                log: Some(Log {
                    address: vec![0xc9],
                    topics: vec![vec![1], vec![2], vec![3]],
                    data: vec![0; 32],
                    block_index: 123,
                }),
                tx_hash: "AB".into(),
            }],
        };
        let data = BlockScopedData {
            output: Some(MapModuleOutput {
                map_output: Some(Any {
                    value: events.encode_to_vec(),
                }),
            }),
            clock: Some(Clock {
                id: "0xFF".into(),
                number: 7,
                timestamp: Some(Timestamp { seconds: 9 }),
            }),
        };
        let mut out = Vec::new();
        collect(data, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].block, out[0].log_index), (7, 123));
        assert_eq!(
            (out[0].block_hash.as_str(), out[0].tx_hash.as_str()),
            ("ff", "ab")
        );
    }
}

/// `SUBSTREAMS_API_KEY=… cargo test -p kittiwake-ext live_mainnet -- --ignored --nocapture`
#[cfg(test)]
mod live {
    use super::*;

    #[tokio::test]
    #[ignore = "needs a GraphOps Substreams key and the network"]
    async fn live_mainnet() {
        let key = std::env::var("SUBSTREAMS_API_KEY").expect("SUBSTREAMS_API_KEY");
        let ss = Substreams::new("https://1.evm.substreams.graphops.xyz", &key);
        let q = "evt_addr:0xc944e90c64b2c07662a292be6244bdf05cda44a7 && \
                 evt_sig:0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        let events = ss
            .filtered_events(q, 26_282_934, 26_290_134, Duration::from_secs(120))
            .await
            .unwrap();
        println!(
            "{} events, first {:?}",
            events.len(),
            events.first().map(|e| (e.block, &e.tx_hash, e.log_index))
        );
        assert_eq!(events.len(), 803);
    }
}
