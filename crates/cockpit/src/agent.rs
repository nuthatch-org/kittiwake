//! The indexer-agent management API, reached only through the operations named here.
//!
//! Queries follow indexer-cli v0.25.11 (`packages/indexer-cli/src/actions.ts`), including its
//! `[Int!]!` action IDs, which is what the agent has been driven with in practice.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

const ACTIONS: &str = "query actions($filter: ActionFilter!, $first: Int, $orderBy: ActionParams, $orderDirection: OrderDirection) {
  actions(filter: $filter, orderBy: $orderBy, orderDirection: $orderDirection, first: $first) {
    id protocolNetwork type allocationID deploymentID amount poi publicPOI poiBlockNumber force
    source reason priority transaction status failureReason createdAt updatedAt isLegacy
  }
}";

const QUEUE: &str = "mutation queueActions($actions: [ActionInput!]!) {
  queueActions(actions: $actions) {
    id type deploymentID allocationID amount force source reason priority status protocolNetwork isLegacy
  }
}";

const APPROVE: &str = "mutation approveActions($actionIDs: [Int!]!) {
  approveActions(actionIDs: $actionIDs) { id type deploymentID allocationID amount status }
}";

const CANCEL: &str = "mutation cancelActions($actionIDs: [Int!]!) {
  cancelActions(actionIDs: $actionIDs) { id type deploymentID allocationID amount status }
}";

const RULES: &str = "query indexingRules($protocolNetwork: String) {
  indexingRules(merged: false, protocolNetwork: $protocolNetwork) {
    identifier identifierType allocationAmount allocationLifetime autoRenewal parallelAllocations
    maxAllocationPercentage minSignal maxSignal minStake minAverageQueryFees custom decisionBasis
    requireSupported safety protocolNetwork
  }
}";

pub const SOURCE: &str = "lodestar-cockpit";
const MAX_BATCH: usize = 50;
const STATUSES: [&str; 7] = [
    "queued",
    "approved",
    "deploying",
    "pending",
    "success",
    "failed",
    "canceled",
];

#[derive(Debug)]
pub enum AgentError {
    Invalid(String),
    Unreachable(String),
    Upstream(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::Invalid(e) => write!(f, "{e}"),
            AgentError::Unreachable(e) => write!(f, "the indexer agent did not answer: {e}"),
            AgentError::Upstream(e) => write!(f, "the indexer agent refused: {e}"),
        }
    }
}

pub struct Agent {
    url: url::Url,
    token: Option<String>,
    network: String,
    http: reqwest::Client,
}

impl Agent {
    /// Refuses a URL the SSRF guard refuses, and pins every connection to the addresses it
    /// checked, so a later DNS answer cannot move the agent somewhere else.
    pub async fn connect(
        raw: &str,
        token: Option<String>,
        network: String,
    ) -> Result<Self, kittiwake_ssrf::Refusal> {
        let (url, addrs) = kittiwake_ssrf::check_configured(raw).await?;
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(30));
        if let Some(host) = url.host_str()
            && host.parse::<std::net::IpAddr>().is_err()
        {
            builder = builder.resolve_to_addrs(host, &addrs);
        }
        let http = builder
            .build()
            .map_err(|e| kittiwake_ssrf::Refusal::Unresolvable(e.to_string()))?;
        Ok(Self {
            url,
            token,
            network,
            http,
        })
    }

    async fn call(
        &self,
        query: &'static str,
        variables: Value,
        field: &str,
    ) -> Result<Value, AgentError> {
        let mut req = self
            .http
            .post(self.url.clone())
            .json(&json!({ "query": query, "variables": variables }));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AgentError::Unreachable(e.without_url().to_string()))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|_| AgentError::Upstream(format!("HTTP {status}, not a GraphQL answer")))?;
        if let Some(errors) = body.get("errors").and_then(Value::as_array)
            && !errors.is_empty()
        {
            let msg = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(AgentError::Upstream(msg));
        }
        if !status.is_success() {
            return Err(AgentError::Upstream(format!("HTTP {status}")));
        }
        Ok(body
            .get("data")
            .and_then(|d| d.get(field))
            .cloned()
            .unwrap_or(Value::Null))
    }

    pub async fn actions(&self, status: Option<&str>) -> Result<Value, AgentError> {
        let mut filter = json!({ "protocolNetwork": self.network });
        if let Some(s) = status {
            if !STATUSES.contains(&s) {
                return Err(AgentError::Invalid(format!("unknown status {s:?}")));
            }
            filter["status"] = json!(s);
        }
        self.call(
            ACTIONS,
            json!({ "filter": filter, "first": 500, "orderBy": "id", "orderDirection": "desc" }),
            "actions",
        )
        .await
    }

    pub async fn queue(&self, actions: &[QueueRequest]) -> Result<Value, AgentError> {
        if actions.is_empty() || actions.len() > MAX_BATCH {
            return Err(AgentError::Invalid(format!(
                "queue between 1 and {MAX_BATCH} actions"
            )));
        }
        let inputs = actions
            .iter()
            .map(|a| a.to_input(&self.network))
            .collect::<Result<Vec<_>, _>>()?;
        self.call(QUEUE, json!({ "actions": inputs }), "queueActions")
            .await
    }

    pub async fn approve(&self, ids: &[i64]) -> Result<Value, AgentError> {
        check_ids(ids)?;
        self.call(APPROVE, json!({ "actionIDs": ids }), "approveActions")
            .await
    }

    pub async fn cancel(&self, ids: &[i64]) -> Result<Value, AgentError> {
        check_ids(ids)?;
        self.call(CANCEL, json!({ "actionIDs": ids }), "cancelActions")
            .await
    }

    pub async fn rules(&self) -> Result<Value, AgentError> {
        self.call(
            RULES,
            json!({ "protocolNetwork": self.network }),
            "indexingRules",
        )
        .await
    }
}

fn check_ids(ids: &[i64]) -> Result<(), AgentError> {
    if ids.is_empty() || ids.len() > MAX_BATCH || ids.iter().any(|i| *i <= 0) {
        return Err(AgentError::Invalid(format!(
            "between 1 and {MAX_BATCH} positive action IDs"
        )));
    }
    Ok(())
}

/// One action as the browser asks for it. Amounts are GRT, as indexer-cli takes them.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueRequest {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "deploymentID")]
    pub deployment_id: String,
    #[serde(rename = "allocationID", default)]
    pub allocation_id: Option<String>,
    #[serde(default)]
    pub amount: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl QueueRequest {
    fn to_input(&self, network: &str) -> Result<Value, AgentError> {
        let bad = |m: String| Err(AgentError::Invalid(m));
        if !is_deployment(&self.deployment_id) {
            return bad(format!("{:?} is not a deployment ID", self.deployment_id));
        }
        let needs_alloc = matches!(self.kind.as_str(), "unallocate" | "reallocate");
        let needs_amount = matches!(self.kind.as_str(), "allocate" | "reallocate");
        if !matches!(self.kind.as_str(), "allocate" | "unallocate" | "reallocate") {
            return bad(format!(
                "{:?} is not an action the Cockpit queues",
                self.kind
            ));
        }
        match (&self.allocation_id, needs_alloc) {
            (Some(a), true) if is_address(a) => {}
            (None, false) => {}
            _ => {
                return bad(format!(
                    "{} needs exactly the allocation it acts on",
                    self.kind
                ));
            }
        }
        match (&self.amount, needs_amount) {
            (Some(a), true) if is_grt(a) => {}
            (None, false) => {}
            _ => return bad(format!("{} needs a positive GRT amount", self.kind)),
        }
        let reason = self.reason.clone().unwrap_or_else(|| "cockpit".into());
        if reason.len() > 200 {
            return bad("reason is limited to 200 characters".into());
        }
        let mut input = json!({
            "status": "queued",
            "type": self.kind,
            "deploymentID": self.deployment_id,
            "source": SOURCE,
            "reason": reason,
            "priority": 0,
            "protocolNetwork": network,
            "isLegacy": false,
        });
        if let Some(a) = &self.allocation_id {
            input["allocationID"] = json!(a.to_lowercase());
            input["force"] = json!(false);
        }
        if let Some(a) = &self.amount {
            input["amount"] = json!(a);
        }
        Ok(input)
    }
}

fn is_deployment(s: &str) -> bool {
    let b58 = |c: char| c.is_ascii_alphanumeric() && !matches!(c, '0' | 'O' | 'I' | 'l');
    (s.len() == 46 && s.starts_with("Qm") && s.chars().all(b58))
        || (s.len() == 66 && s.starts_with("0x") && s[2..].chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_address(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && s[2..].chars().all(|c| c.is_ascii_hexdigit())
}

fn is_grt(s: &str) -> bool {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    !int.is_empty()
        && int.len() <= 12
        && frac.len() <= 18
        && int.chars().chain(frac.chars()).all(|c| c.is_ascii_digit())
        && s.chars().any(|c| c.is_ascii_digit() && c != '0')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(kind: &str, alloc: Option<&str>, amount: Option<&str>) -> QueueRequest {
        QueueRequest {
            kind: kind.into(),
            deployment_id: "QmXZiV6S13ha6QXq4dmaM3TB4CHcDxBMvGexSNu9Kc28EH".into(),
            allocation_id: alloc.map(Into::into),
            amount: amount.map(Into::into),
            reason: None,
        }
    }

    const ALLOC: &str = "0x1111111111111111111111111111111111111111";

    #[test]
    fn each_action_takes_exactly_its_arguments() {
        assert!(req("allocate", None, Some("1000")).to_input("n").is_ok());
        assert!(req("unallocate", Some(ALLOC), None).to_input("n").is_ok());
        assert!(
            req("reallocate", Some(ALLOC), Some("2500.5"))
                .to_input("n")
                .is_ok()
        );

        assert!(
            req("allocate", Some(ALLOC), Some("1000"))
                .to_input("n")
                .is_err()
        );
        assert!(req("unallocate", None, None).to_input("n").is_err());
        assert!(
            req("unallocate", Some(ALLOC), Some("1"))
                .to_input("n")
                .is_err()
        );
        assert!(req("reallocate", Some(ALLOC), None).to_input("n").is_err());
        assert!(req("presentPOI", Some(ALLOC), None).to_input("n").is_err());
        assert!(req("resize", Some(ALLOC), Some("1")).to_input("n").is_err());
    }

    #[test]
    fn amounts_are_positive_decimal_grt() {
        for ok in ["1", "0.5", "1000000", "12.000000000000000001"] {
            assert!(is_grt(ok), "{ok}");
        }
        for bad in [
            "0",
            "0.0",
            "-1",
            "1e18",
            "",
            ".5",
            "1,000",
            "1.0000000000000000001",
        ] {
            assert!(!is_grt(bad), "{bad}");
        }
    }

    #[test]
    fn an_unallocate_is_queued_without_a_poi_so_the_agent_computes_it() {
        let v = req("unallocate", Some(ALLOC), None)
            .to_input("eip155:42161")
            .unwrap();
        assert!(v.get("poi").is_none());
        assert_eq!(v["force"], json!(false));
        assert_eq!(v["source"], json!(SOURCE));
        assert_eq!(v["protocolNetwork"], json!("eip155:42161"));
    }
}
