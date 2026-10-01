//! Every JSON-RPC request this crate sends, counted by chain, the operation that sent it and method.
//!
//! The RPC provider bills per key and per method, never per caller, so without `op` a burst of
//! `eth_call` cannot be traced to the code that made it.

use std::task::{Context, Poll};

use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::client::ClientBuilder;
use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::http::reqwest::Url;
use alloy::transports::{TransportError, TransportFut};
use tower::{Layer, Service};

pub(crate) fn connect(url: Url, chain: &'static str, op: &'static str) -> impl Provider + Clone {
    let client = ClientBuilder::default()
        .layer(Metered { chain, op })
        .http(url);
    ProviderBuilder::new().connect_client(client)
}

#[derive(Clone, Copy)]
struct Metered {
    chain: &'static str,
    op: &'static str,
}

impl<S> Layer<S> for Metered {
    type Service = MeteredService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MeteredService {
            inner,
            labels: *self,
        }
    }
}

#[derive(Clone)]
struct MeteredService<S> {
    inner: S,
    labels: Metered,
}

impl<S> Service<RequestPacket> for MeteredService<S>
where
    S: Service<
            RequestPacket,
            Response = ResponsePacket,
            Error = TransportError,
            Future = TransportFut<'static>,
        >,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        // A batch is one HTTP request but one billed call per item, so each item counts.
        for method in request.method_names() {
            metrics::counter!(
                "rpc_requests_total",
                "chain" => self.labels.chain,
                "op" => self.labels.op,
                "method" => method.to_owned(),
            )
            .increment(1);
        }
        self.inner.call(request)
    }
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;

    /// Answers any single request or batch with `0x1` per item, echoing the ids.
    fn answer(req: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        let one = |r: &serde_json::Value| serde_json::json!({"jsonrpc": "2.0", "id": r["id"], "result": "0x1"});
        match body.as_array() {
            Some(items) => {
                ResponseTemplate::new(200).set_body_json(items.iter().map(one).collect::<Vec<_>>())
            }
            None => ResponseTemplate::new(200).set_body_json(one(&body)),
        }
    }

    #[test]
    fn each_request_and_each_batch_item_is_counted_under_its_op() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let server = MockServer::start().await;
                    Mock::given(method("POST"))
                        .respond_with(answer)
                        .mount(&server)
                        .await;
                    let url: Url = server.uri().parse().unwrap();

                    connect(url.clone(), "arbitrum", "block_number")
                        .get_block_number()
                        .await
                        .unwrap();

                    let client = ClientBuilder::default()
                        .layer(Metered {
                            chain: "mainnet",
                            op: "batched",
                        })
                        .http(url);
                    let mut batch = client.new_batch();
                    let a = batch.add_call::<_, String>("eth_chainId", &()).unwrap();
                    let b = batch.add_call::<_, String>("eth_chainId", &()).unwrap();
                    batch.send().await.unwrap();
                    a.await.unwrap();
                    b.await.unwrap();
                });
        });

        let rendered = handle.render();
        for line in [
            r#"rpc_requests_total{chain="arbitrum",op="block_number",method="eth_blockNumber"} 1"#,
            r#"rpc_requests_total{chain="mainnet",op="batched",method="eth_chainId"} 2"#,
        ] {
            assert!(rendered.contains(line), "{line} missing from:\n{rendered}");
        }
    }
}
