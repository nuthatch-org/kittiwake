//! `/api/subgraph-directory`: every listed deployment, filtered, sorted and paged where the whole
//! set is.
//!
//! The directory used to filter in the browser over whichever 25 rows the server had paged, so a
//! filtered view silently missed every match on the other pages, and a row whose manifest had not
//! loaded yet passed every network filter (lodestar#252). The set is small enough to hold: 5,594
//! deployments above the signal floor on 19 September 2026. So it is fetched and labelled once per
//! refresh, and each request filters it in memory.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

/// The largest page a caller may ask for.
pub const MAX_PAGE: usize = 100;
const DEFAULT_PAGE: usize = 25;

/// Which fee figure the fee filter and sort read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Window {
    #[default]
    Days30,
    AllTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    Signal,
    Stake,
    #[default]
    Fees,
    Created,
    Ratio,
    Indexers,
    Curators,
}

/// Inclusive bounds; an absent side is open.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Range {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl Range {
    fn admits(&self, v: f64) -> bool {
        self.min.is_none_or(|m| v >= m) && self.max.is_none_or(|m| v <= m)
    }
}

/// Whether the RewardsManager denylist filters the set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Denied {
    #[default]
    Any,
    Hide,
    Only,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub window: Window,
    /// GRT.
    pub signal: Range,
    /// GRT.
    pub stake: Range,
    /// Signal over allocated stake.
    pub ratio: Range,
    /// GRT, in the chosen window.
    pub fees: Range,
    pub indexers: Range,
    pub network: Option<String>,
    pub complexity: Option<String>,
    pub category: Option<String>,
    pub created_within_days: Option<i64>,
    pub denied: Denied,
    pub sort: SortKey,
    pub descending: bool,
    pub first: usize,
    pub skip: usize,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            window: Window::default(),
            signal: Range::default(),
            stake: Range::default(),
            ratio: Range::default(),
            fees: Range::default(),
            indexers: Range::default(),
            network: None,
            complexity: None,
            category: None,
            created_within_days: None,
            denied: Denied::default(),
            sort: SortKey::default(),
            descending: true,
            first: DEFAULT_PAGE,
            skip: 0,
        }
    }
}

impl Query {
    /// From the query string. A value that cannot be read is refused rather than ignored: a bound
    /// silently dropped widens the answer, and the caller would take it for a filtered one.
    pub fn parse(params: &HashMap<String, String>) -> Result<Self, String> {
        let get = |k: &str| params.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
        let number = |k: &str| -> Result<Option<f64>, String> {
            match get(k) {
                None => Ok(None),
                Some(v) => match v.parse::<f64>() {
                    Ok(n) if n.is_finite() && n >= 0.0 => Ok(Some(n)),
                    _ => Err(format!("{k} must be a non-negative number")),
                },
            }
        };
        let range = |k: &str| -> Result<Range, String> {
            Ok(Range {
                min: number(&format!("{k}Min"))?,
                max: number(&format!("{k}Max"))?,
            })
        };
        let text = |k: &str| -> Result<Option<String>, String> {
            match get(k) {
                Some(v) if v.len() > 100 => Err(format!("{k} is too long")),
                v => Ok(v.map(str::to_string)),
            }
        };
        let whole = |k: &str, lo: usize, hi: usize, default: usize| -> Result<usize, String> {
            match get(k) {
                None => Ok(default),
                Some(v) => v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (lo..=hi).contains(n))
                    .ok_or_else(|| format!("{k} must be a whole number from {lo} to {hi}")),
            }
        };

        let window = match get("window") {
            None | Some("30d") => Window::Days30,
            Some("allTime") => Window::AllTime,
            Some(_) => return Err("window must be 30d or allTime".into()),
        };
        let sort = match get("sort") {
            None | Some("queryFees") => SortKey::Fees,
            Some("signal") => SortKey::Signal,
            Some("stake") => SortKey::Stake,
            Some("created") => SortKey::Created,
            Some("ratio") => SortKey::Ratio,
            Some("indexers") => SortKey::Indexers,
            Some("curators") => SortKey::Curators,
            Some(_) => return Err("sort is not a column the directory sorts by".into()),
        };
        let denied = match get("denied") {
            None => Denied::Any,
            Some("hide") => Denied::Hide,
            Some("only") => Denied::Only,
            Some(_) => return Err("denied must be hide or only".into()),
        };
        let descending = match get("dir") {
            None | Some("desc") => true,
            Some("asc") => false,
            Some(_) => return Err("dir must be asc or desc".into()),
        };

        Ok(Self {
            window,
            signal: range("signal")?,
            stake: range("stake")?,
            ratio: range("ratio")?,
            fees: range("fees")?,
            indexers: range("indexers")?,
            network: text("network")?,
            complexity: text("complexity")?,
            category: text("category")?,
            created_within_days: get("createdWithinDays")
                .map(|_| whole("createdWithinDays", 1, 3650, 0).map(|d| d as i64))
                .transpose()?,
            denied,
            sort,
            descending,
            first: whole("first", 1, MAX_PAGE, DEFAULT_PAGE)?,
            skip: whole("skip", 0, 100_000, 0)?,
        })
    }
}

fn grt(row: &Value, key: &str) -> f64 {
    row.get(key)
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .map_or(0.0, |wei| wei / 1e18)
}

fn count(row: &Value, key: &str) -> f64 {
    row.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Every row carries at least the signal floor, so no stake is an unbounded ratio rather than an
/// undefined one: nothing allocated against real signal is as under-allocated as a deployment gets.
fn ratio(row: &Value) -> f64 {
    let stake = grt(row, "stakedTokens");
    if stake > 0.0 {
        grt(row, "signalledTokens") / stake
    } else {
        f64::INFINITY
    }
}

fn fees(row: &Value, window: Window) -> f64 {
    match window {
        Window::Days30 => grt(row, "queryFees30d"),
        Window::AllTime => grt(row, "queryFeesAmount"),
    }
}

fn text<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

fn sort_value(row: &Value, q: &Query) -> f64 {
    match q.sort {
        SortKey::Signal => grt(row, "signalledTokens"),
        SortKey::Stake => grt(row, "stakedTokens"),
        SortKey::Fees => fees(row, q.window),
        SortKey::Created => count(row, "createdAt"),
        SortKey::Ratio => ratio(row),
        SortKey::Indexers => count(row, "indexerCount"),
        SortKey::Curators => count(row, "curatorCount"),
    }
}

fn admits(row: &Value, q: &Query, now: i64) -> bool {
    q.signal.admits(grt(row, "signalledTokens"))
        && q.stake.admits(grt(row, "stakedTokens"))
        && q.ratio.admits(ratio(row))
        && q.fees.admits(fees(row, q.window))
        && q.indexers.admits(count(row, "indexerCount"))
        // An unread manifest has no network, so it matches no network: passing it would put rows
        // in a filtered view that nobody has shown belong there.
        && q.network.as_deref().is_none_or(|n| text(row, "network") == Some(n))
        && q.complexity.as_deref().is_none_or(|c| text(row, "complexity") == Some(c))
        && q.category.as_deref().is_none_or(|c| {
            row.get("categories")
                .and_then(Value::as_array)
                .is_some_and(|cs| cs.iter().any(|x| x.as_str() == Some(c)))
        })
        && q.created_within_days.is_none_or(|d| count(row, "createdAt") >= (now - d * 86_400) as f64)
        && match q.denied {
            Denied::Any => true,
            Denied::Hide => !is_denied(row),
            Denied::Only => is_denied(row),
        }
}

fn is_denied(row: &Value) -> bool {
    row.get("deniedAt").is_some_and(|v| !v.is_null())
}

/// One page of the rows that match, and how many match in all.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub rows: Vec<Value>,
    pub total: usize,
}

/// Filter, sort and page `set`, the value [`set`] built.
pub fn select(set: &Value, q: &Query, now: i64) -> Page {
    let empty = Vec::new();
    let rows = set.get("rows").and_then(Value::as_array).unwrap_or(&empty);
    let mut hits: Vec<(f64, &Value)> = rows
        .iter()
        .filter(|r| admits(r, q, now))
        .map(|r| (sort_value(r, q), r))
        .collect();
    hits.sort_by(|(a, ra), (b, rb)| {
        let by_value = if q.descending {
            b.total_cmp(a)
        } else {
            a.total_cmp(b)
        };
        // Ties go by id so a page boundary never shuffles between two requests.
        by_value.then_with(|| text(ra, "id").cmp(&text(rb, "id")))
    });
    Page {
        total: hits.len(),
        rows: hits
            .into_iter()
            .skip(q.skip)
            .take(q.first)
            .map(|(_, r)| r.clone())
            .collect(),
    }
}

/// One directory row from a `lodestar_deployments` row and its fees in the window.
///
/// The labels (`displayName`, `categories`, `network`, `complexity`) start empty and are put on by
/// [`crate::naming::Naming::label_directory`].
pub fn row(r: &Value, fees_in_window: Option<&Value>) -> Value {
    use crate::shape::{i0, ipfs_or_id, s0};
    let id = r.get("id").and_then(Value::as_str).unwrap_or_default();
    json!({
        "id": id,
        "ipfsHash": ipfs_or_id(id),
        "signalledTokens": s0(r, "signalled_tokens"),
        "stakedTokens": s0(r, "staked_tokens"),
        "queryFeesAmount": s0(r, "query_fees_amount"),
        "queryFees30d": match fees_in_window {
            Some(Value::String(s)) => json!(s),
            Some(Value::Number(n)) => json!(n.to_string()),
            _ => json!("0"),
        },
        "createdAt": i0(r, "created_at"),
        // The block denial took effect from; null while the deployment earns rewards.
        "deniedAt": crate::shape::i(r, "denied_since"),
        "indexerCount": i0(r, "active_allocation_count"),
        "curatorCount": i0(r, "curator_count"),
        "displayName": Value::Null,
        "categories": [],
        "network": Value::Null,
        "complexity": Value::Null,
    })
}

/// The cached value: the labelled rows, what their filters can be set to, and how many rows have
/// no manifest read yet and so cannot match a network or complexity filter.
pub fn set(rows: Vec<Value>, unanalysed: usize) -> Value {
    let mut networks: BTreeMap<&str, usize> = BTreeMap::new();
    let mut complexities: BTreeMap<&str, usize> = BTreeMap::new();
    let mut categories: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &rows {
        if let Some(n) = text(r, "network") {
            *networks.entry(n).or_default() += 1;
        }
        if let Some(c) = text(r, "complexity") {
            *complexities.entry(c).or_default() += 1;
        }
        for c in r
            .get("categories")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            *categories.entry(c).or_default() += 1;
        }
    }
    let listed = |m: BTreeMap<&str, usize>| -> Vec<Value> {
        let mut v: Vec<(&str, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        v.into_iter()
            .map(|(id, count)| json!({ "id": id, "count": count }))
            .collect()
    };
    let facets = json!({
        "networks": listed(networks),
        "complexities": listed(complexities),
        "categories": listed(categories),
    });
    let denied = rows.iter().filter(|r| is_denied(r)).count();
    json!({ "rows": rows, "unanalysed": unanalysed, "denied": denied, "facets": facets })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRT: u128 = 1_000_000_000_000_000_000;
    const NOW: i64 = 1_790_000_000;

    fn dep(id: &str, signal: u128, stake: u128) -> Value {
        json!({
            "id": id,
            "signalledTokens": (signal * GRT).to_string(),
            "stakedTokens": (stake * GRT).to_string(),
            "queryFeesAmount": "0",
            "queryFees30d": "0",
            "createdAt": NOW - 400 * 86_400,
            "indexerCount": if stake > 0 { 3 } else { 0 },
            "curatorCount": 1,
            "deniedAt": null,
            "displayName": null,
            "categories": [],
            "network": null,
            "complexity": null,
        })
    }

    fn with(mut r: Value, k: &str, v: Value) -> Value {
        r[k] = v;
        r
    }

    fn ids(p: &Page) -> Vec<&str> {
        p.rows.iter().map(|r| r["id"].as_str().unwrap()).collect()
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn q(pairs: &[(&str, &str)]) -> Query {
        Query::parse(&params(pairs)).unwrap()
    }

    /// The fault #252 names: a match on page 40 of the unfiltered list is still a match.
    #[test]
    fn a_filter_sees_the_whole_set_and_counts_every_match() {
        let mut rows: Vec<Value> = (0..1000)
            .map(|i| dep(&format!("0x{i:04}"), 10, 1_000))
            .collect();
        rows.push(dep("0xfar", 300, 10));
        let set = set(rows, 0);

        let p = select(&set, &q(&[("ratioMin", "5"), ("signalMin", "200")]), NOW);
        assert_eq!(p.total, 1);
        assert_eq!(ids(&p), ["0xfar"]);
    }

    /// mindstyle's screen, read as #250 settled it: signal over allocated stake.
    #[test]
    fn the_ratio_is_signal_over_stake_and_nothing_allocated_is_unbounded() {
        let set = set(
            vec![
                dep("0xunder", 600, 100),
                dep("0xover", 200, 10_000),
                dep("0xbare", 250, 0),
            ],
            0,
        );
        let p = select(
            &set,
            &q(&[("ratioMin", "5"), ("signalMin", "200"), ("sort", "ratio")]),
            NOW,
        );
        assert_eq!(ids(&p), ["0xbare", "0xunder"]);

        // An upper bound is still a bound on the unbounded one.
        let p = select(&set, &q(&[("ratioMax", "10")]), NOW);
        assert_eq!(ids(&p).len(), 2);
        assert!(!ids(&p).contains(&"0xbare"));
    }

    #[test]
    fn an_unread_manifest_matches_no_network_and_no_complexity() {
        let set = set(
            vec![
                with(dep("0xa", 10, 10), "network", json!("mainnet")),
                with(dep("0xb", 10, 10), "network", json!("arbitrum-one")),
                dep("0xunread", 10, 10),
            ],
            1,
        );
        let p = select(&set, &q(&[("network", "mainnet")]), NOW);
        assert_eq!(ids(&p), ["0xa"]);
        let p = select(&set, &q(&[("complexity", "Light")]), NOW);
        assert_eq!(p.total, 0);
    }

    #[test]
    fn denied_deployments_can_be_hidden_or_picked_out() {
        let set = set(
            vec![
                with(dep("0xdenied", 10, 10), "deniedAt", json!(250_000_000)),
                dep("0xearning", 10, 10),
            ],
            0,
        );
        assert_eq!(set["denied"], 1);
        assert_eq!(select(&set, &q(&[]), NOW).total, 2);
        assert_eq!(
            ids(&select(&set, &q(&[("denied", "hide")]), NOW)),
            ["0xearning"]
        );
        assert_eq!(
            ids(&select(&set, &q(&[("denied", "only")]), NOW)),
            ["0xdenied"]
        );
    }

    #[test]
    fn fees_are_read_in_the_window_asked_for() {
        let a = with(
            with(
                dep("0xa", 10, 10),
                "queryFees30d",
                json!((50 * GRT).to_string()),
            ),
            "queryFeesAmount",
            json!((60 * GRT).to_string()),
        );
        let b = with(
            dep("0xb", 10, 10),
            "queryFeesAmount",
            json!((5_000 * GRT).to_string()),
        );
        let set = set(vec![a, b], 0);

        assert_eq!(
            ids(&select(&set, &q(&[("feesMin", "1000")]), NOW)),
            Vec::<&str>::new()
        );
        assert_eq!(
            ids(&select(
                &set,
                &q(&[("feesMin", "1000"), ("window", "allTime")]),
                NOW
            )),
            ["0xb"]
        );
        assert_eq!(ids(&select(&set, &q(&[]), NOW)), ["0xa", "0xb"]);
    }

    #[test]
    fn categories_and_age_filter_the_set() {
        let set = set(
            vec![
                with(
                    with(dep("0xnew", 10, 10), "createdAt", json!(NOW - 3 * 86_400)),
                    "categories",
                    json!(["DeFi", "NFTs"]),
                ),
                with(dep("0xold", 10, 10), "categories", json!(["DeFi"])),
            ],
            0,
        );
        assert_eq!(
            ids(&select(&set, &q(&[("category", "NFTs")]), NOW)),
            ["0xnew"]
        );
        assert_eq!(
            ids(&select(&set, &q(&[("createdWithinDays", "30")]), NOW)),
            ["0xnew"]
        );
        assert_eq!(select(&set, &q(&[("category", "DeFi")]), NOW).total, 2);
    }

    #[test]
    fn pages_are_stable_under_ties_and_the_total_ignores_the_page() {
        let rows: Vec<Value> = ["0xc", "0xa", "0xb", "0xd"]
            .iter()
            .map(|id| dep(id, 10, 10))
            .collect();
        let set = set(rows, 0);
        let first = select(&set, &q(&[("first", "2"), ("sort", "signal")]), NOW);
        let second = select(
            &set,
            &q(&[("first", "2"), ("skip", "2"), ("sort", "signal")]),
            NOW,
        );
        assert_eq!(ids(&first), ["0xa", "0xb"]);
        assert_eq!(ids(&second), ["0xc", "0xd"]);
        assert_eq!((first.total, second.total), (4, 4));
    }

    #[test]
    fn ascending_reverses_the_order_and_keeps_ties_by_id() {
        let set = set(vec![dep("0xbig", 900, 10), dep("0xsmall", 20, 10)], 0);
        let p = select(&set, &q(&[("sort", "signal"), ("dir", "asc")]), NOW);
        assert_eq!(ids(&p), ["0xsmall", "0xbig"]);
    }

    #[test]
    fn a_value_that_cannot_be_read_is_refused_not_dropped() {
        for bad in [
            ("signalMin", "lots"),
            ("ratioMin", "-1"),
            ("feesMax", "inf"),
            ("first", "0"),
            ("first", "101"),
            ("window", "7d"),
            ("sort", "name"),
            ("dir", "up"),
            ("createdWithinDays", "0"),
            ("denied", "yes"),
        ] {
            assert!(
                Query::parse(&params(&[bad])).is_err(),
                "{bad:?} was accepted"
            );
        }
        // Empty is absent, which is what a cleared form field sends.
        assert_eq!(q(&[("signalMin", "")]), Query::default());
    }

    #[test]
    fn the_facets_count_the_whole_set() {
        let set = set(
            vec![
                with(
                    with(dep("0xa", 10, 10), "network", json!("mainnet")),
                    "complexity",
                    json!("Light"),
                ),
                with(dep("0xb", 10, 10), "network", json!("mainnet")),
                with(dep("0xc", 10, 10), "network", json!("base")),
            ],
            0,
        );
        assert_eq!(
            set["facets"]["networks"],
            json!([{ "id": "mainnet", "count": 2 }, { "id": "base", "count": 1 }])
        );
        assert_eq!(
            set["facets"]["complexities"],
            json!([{ "id": "Light", "count": 1 }])
        );
    }

    #[test]
    fn a_nest_row_becomes_a_directory_row_with_its_window_fees() {
        let r = row(
            &json!({
                "id": "0xc915fae50b8dc6976b454869031c52761538b1354ff5587a207dbdbbe66e73fa",
                "signalled_tokens": "249408684318053136427043",
                "staked_tokens": "1000000000000000000000000",
                "query_fees_amount": "2016227210497306645887066",
                "created_at": 1758745880,
                "active_allocation_count": 1,
                "curator_count": 2,
            }),
            Some(&json!("5000")),
        );
        assert_eq!(
            r["ipfsHash"],
            "Qmbsc6XQWbiv4DfLVfaNciScqYLyDWUYjWzrFBbzzmRsMB"
        );
        assert_eq!(r["queryFees30d"], "5000");
        assert_eq!(r["indexerCount"], 1);
        assert_eq!(r["network"], Value::Null);
        assert_eq!(r["deniedAt"], Value::Null);

        let bare = row(&json!({ "id": "0x00", "denied_since": "250000000" }), None);
        assert_eq!(bare["queryFees30d"], "0");
        assert_eq!(bare["deniedAt"], 250_000_000);
    }
}
