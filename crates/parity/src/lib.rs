//! Comparing two APIs by shape, not by value.
//!
//! Values legitimately differ between two calls a second apart: a block advances, an allocation
//! closes, a price moves. Asserting on values would produce a harness that fails constantly and is
//! therefore ignored, which is worse than no harness. What must not differ is the **shape**: the
//! set of paths the JSON contains and the type at each one, because that is what the frontend's
//! hooks are written against.
//!
//! A shape here is a flat map from path to the set of types observed there. Array indices collapse
//! to `[]`, so a hundred rows describe one shape, and an optional field shows up honestly as
//! `{string, null}` rather than as two incompatible shapes.

use std::collections::{BTreeMap, BTreeSet};

pub type Shape = BTreeMap<String, BTreeSet<String>>;

/// The type tag for one JSON value.
///
/// Numbers carried as strings get their own tag. The protocol deals in wei, which does not survive
/// an `f64`, so the whole API passes big values as decimal strings - and a field that silently
/// changed from `"1000"` to `1000` would break every consumer that parses it.
fn tag(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        // One tag for every JSON number. Splitting int from float would report `3` against `3.0`
        // as a break, and the consumers are JavaScript, which has a single number type and cannot
        // tell them apart. A TVL that happens to land on a whole number is not a contract change.
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(s) => {
            if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
                "numeric-string"
            } else {
                "string"
            }
        }
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

pub fn shape_of(v: &serde_json::Value) -> Shape {
    let mut out = Shape::new();
    walk("", v, &mut out);
    out
}

fn walk(path: &str, v: &serde_json::Value, out: &mut Shape) {
    out.entry(if path.is_empty() {
        ".".into()
    } else {
        path.to_string()
    })
    .or_default()
    .insert(tag(v).to_string());

    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map {
                walk(&format!("{path}.{k}"), child, out);
            }
        }
        serde_json::Value::Array(items) => {
            // Every element folds into one path, so a long list costs one entry and an
            // inconsistent list shows up as a union rather than as noise.
            for item in items.iter().take(200) {
                walk(&format!("{path}[]"), item, out);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Default)]
pub struct Diff {
    /// In the incumbent, absent from ours. These break consumers.
    pub missing: Vec<String>,
    /// In ours, absent from the incumbent. Usually harmless: consumers tolerate extra keys.
    pub extra: Vec<String>,
    /// Present in both, different types. These break consumers silently.
    pub retyped: Vec<(String, String, String)>,
}

impl Diff {
    /// Anything that would break a consumer written against the incumbent.
    pub fn breaking(&self) -> bool {
        !self.missing.is_empty() || !self.retyped.is_empty()
    }
}

/// Compare ours against the incumbent.
///
/// `ignore` are path prefixes to skip: the provenance block we add is expected to be absent from
/// the incumbent and is not a difference anyone needs told about.
pub fn diff(incumbent: &Shape, ours: &Shape, ignore: &[&str]) -> Diff {
    let ignored = |p: &str| ignore.iter().any(|i| p.starts_with(i));
    let mut d = Diff::default();

    for (path, types) in incumbent {
        if ignored(path) {
            continue;
        }
        match ours.get(path) {
            None => d.missing.push(path.clone()),
            Some(mine) => {
                // A union that covers the incumbent's types is fine: it means we return a
                // superset, such as allowing null where they never happened to send one.
                if !types.is_subset(mine) && mine.is_disjoint(types) {
                    d.retyped.push((
                        path.clone(),
                        types.iter().cloned().collect::<Vec<_>>().join("|"),
                        mine.iter().cloned().collect::<Vec<_>>().join("|"),
                    ));
                }
            }
        }
    }

    for path in ours.keys() {
        if !ignored(path) && !incumbent.contains_key(path) {
            d.extra.push(path.clone());
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(json: &str) -> Shape {
        shape_of(&serde_json::from_str(json).unwrap())
    }

    #[test]
    fn a_long_list_describes_one_shape() {
        let sh = s(r#"{"rows":[{"a":1},{"a":2},{"a":3}]}"#);
        assert_eq!(sh[".rows[].a"], BTreeSet::from(["number".to_string()]));
        assert_eq!(sh.len(), 4, "root, rows, rows[], rows[].a");
    }

    #[test]
    fn an_optional_field_is_a_union_not_two_shapes() {
        let sh = s(r#"{"rows":[{"a":"x"},{"a":null}]}"#);
        assert_eq!(
            sh[".rows[].a"],
            BTreeSet::from(["string".to_string(), "null".to_string()])
        );
    }

    /// Wei as a string is not the same as wei as a number, and the difference is silent.
    #[test]
    fn a_numeric_string_is_told_apart_from_a_number() {
        let a = s(r#"{"tokens":"1000"}"#);
        let b = s(r#"{"tokens":1000}"#);
        let d = diff(&a, &b, &[]);
        assert!(d.breaking(), "a string-to-number change must be reported");
        assert_eq!(d.retyped[0].0, ".tokens");
    }

    /// A figure that happens to land on a whole number is not a contract change. Reporting it as
    /// one produces breaks nobody can act on, and a harness people learn to ignore.
    #[test]
    fn a_whole_number_float_is_not_a_break() {
        let incumbent = s(r#"{"tvl": 2612345678.5}"#);
        let ours = s(r#"{"tvl": 2612345678.0}"#);
        assert!(!diff(&incumbent, &ours, &[]).breaking());
    }

    #[test]
    fn a_missing_field_breaks_and_an_extra_one_does_not() {
        let incumbent = s(r#"{"a":1,"b":2}"#);
        let ours = s(r#"{"a":1,"c":3}"#);
        let d = diff(&incumbent, &ours, &[]);
        assert_eq!(d.missing, vec![".b"]);
        assert_eq!(d.extra, vec![".c"]);
        assert!(d.breaking());
    }

    #[test]
    fn our_added_provenance_is_not_a_difference() {
        let incumbent = s(r#"{"data":{"a":1}}"#);
        let ours = s(r#"{"data":{"a":1},"provenance":{"as_of":1}}"#);
        let d = diff(&incumbent, &ours, &[".provenance"]);
        assert!(!d.breaking());
        assert!(d.extra.is_empty());
    }

    /// Returning a superset is not a break: allowing null where they never sent one is safe.
    #[test]
    fn a_wider_type_is_not_a_break() {
        let incumbent = s(r#"{"a":"x"}"#);
        let ours = s(r#"{"a":null}"#);
        let widened = diff(&incumbent, &ours, &[]);
        assert!(widened.breaking(), "narrowing to only-null is a break");

        let mut ours_union = s(r#"{"a":"x"}"#);
        ours_union.get_mut(".a").unwrap().insert("null".into());
        assert!(!diff(&incumbent, &ours_union, &[]).breaking());
    }
}

/// Every route the edge serves must be compared, or excused in writing.
///
/// This is the check that was missing on 2026-09-07, when three deployment routes went into the
/// rewrite without ever being in the harness table. They answered `200` with an entirely different
/// payload and did so in production for an hour, because nothing connected "this service serves it"
/// to "something checks it".
///
/// The two lists live in the same repository, so the connection can simply be asserted. The router
/// source is read at compile time and every `.route("/api/…")` in it must appear either in the
/// harness table or in [`UNCHECKED`] with a reason. Adding a route without doing one or the other
/// fails the build, which is the point: an unchecked route should be a decision somebody wrote
/// down, not an omission nobody noticed.
pub mod coverage {
    /// The router, as source. Read at compile time rather than by enumerating axum's routes,
    /// because axum does not expose them and a test that cannot see the truth is decoration.
    const ROUTER: &str = include_str!("../../edge/src/lib.rs");

    /// How completely a route is excused.
    ///
    /// The distinction is the one that cost the dashboard a day on 2026-09-07. An exclusion argued
    /// on **data** grounds does not reach the **shape**: the envelope and the field names are this
    /// service's own choice whatever table the rows came from, and they are precisely what a
    /// consumer destructures.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Excused {
        /// Nothing to compare against, or the harness cannot make the request at all.
        Entirely,
        /// Values prove nothing, so only the shape is compared. A route excused this way must
        /// still appear in the harness table, and a test below asserts it.
        ValuesOnly,
    }

    /// Routes the harness deliberately does not compare, each with the reason.
    ///
    /// Every entry here is a thing somebody chose. Empty would be better; a long list is a smell.
    pub const UNCHECKED: &[(&str, Excused, &str)] = &[
        (
            "/api/studio/auth",
            Excused::Entirely,
            "session cookies and wallet signatures make the frozen public incumbent unsuitable as a fixture; covered by Dock compatibility tests",
        ),
        (
            "/api/studio/bounties",
            Excused::Entirely,
            "mutates shared Postgres and its public rows are live state, so parity must run against the isolated Dock integration database",
        ),
        (
            "/api/studio/bounties/{id}",
            Excused::Entirely,
            "reads BountyBoard and reconciles live shared-Postgres state, so it requires the Dock integration fixture",
        ),
        (
            "/api/studio/deploy-key",
            Excused::Entirely,
            "returns a secret exactly once and cannot be compared against a frozen deployment",
        ),
        (
            "/api/studio/metadata",
            Excused::Entirely,
            "authenticated IPFS writes create content-addressed live state and require the Dock integration fixture",
        ),
        (
            "/api/studio/node",
            Excused::Entirely,
            "deploy-key-authenticated JSON-RPC mutates shared Studio state and requires the Dock integration fixture",
        ),
        (
            "/api/studio/ipfs/{*path}",
            Excused::Entirely,
            "deploy-key-authenticated IPFS writes are streaming live state and require an isolated upstream fixture",
        ),
        (
            "/api/studio/subgraphs",
            Excused::Entirely,
            "authenticated shared-Postgres mutations require the Dock integration fixture rather than the public parity target",
        ),
        (
            "/api/studio/subgraphs/{id}",
            Excused::Entirely,
            "authenticated shared-Postgres mutations require the Dock integration fixture rather than the public parity target",
        ),
        (
            "/api/whoami",
            Excused::Entirely,
            "a diagnostic that reports the caller's own address, so it has no incumbent to match",
        ),
        (
            "/api/sql/query",
            Excused::Entirely,
            "POST with a body; the harness only issues GETs",
        ),
        (
            "/api/indexer-node-health",
            Excused::Entirely,
            "probes a third party's endpoint, so the answer depends on that host rather than on us",
        ),
        (
            "/api/support/{number}",
            Excused::Entirely,
            "the same mirror as /api/support, one thread at a time; the dashboard has no incumbent \
             handler for it at all",
        ),
        (
            "/api/support",
            Excused::Entirely,
            "has no incumbent to match: the dashboard's own handler reads GitHub live and this \
             reads a mirror written twice a day, so the two disagree by design and the harness \
             would be comparing freshness rather than correctness",
        ),
        (
            "/api/subgraph-names",
            Excused::Entirely,
            "POST with a body; the harness only issues GETs",
        ),
        (
            "/api/indexers-enriched",
            Excused::ValuesOnly,
            "reads rows the platform's own crons write, so both sides read the same table and \
             matching values prove nothing about this service - but the envelope and the field \
             names are ours, and a rename is what broke the dashboard on 2026-09-07",
        ),
    ];

    /// The `/api/...` paths the edge registers, with axum's `{param}` segments left as they are.
    ///
    /// Whitespace after `.route(` is skipped, because rustfmt wraps a long registration onto its
    /// own line and a matcher for `.route("` alone silently misses every one of those. The first
    /// version of this did exactly that and quietly excused four routes; the test below is what
    /// caught it.
    pub fn served_paths() -> Vec<String> {
        let mut out = Vec::new();
        for (i, _) in ROUTER.match_indices(".route(") {
            let rest = ROUTER[i + ".route(".len()..].trim_start();
            let Some(rest) = rest.strip_prefix('"') else {
                continue;
            };
            if let Some(end) = rest.find('"') {
                let p = &rest[..end];
                if p.starts_with("/api/") {
                    out.push(p.to_string());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::coverage::*;

    /// Sanity: if the extraction breaks, every other assertion here passes vacuously.
    #[test]
    fn the_router_is_readable_and_has_routes_in_it() {
        let served = served_paths();
        assert!(
            served.len() > 25,
            "only found {} routes; the extraction is broken, not the router",
            served.len()
        );
        assert!(served.contains(&"/api/network-stats".to_string()));
    }

    /// rustfmt puts a long registration's path on its own line. Missing those is invisible: the
    /// route is served, the extractor does not see it, and it is excused from comparison by
    /// accident rather than by decision. This is one that is written that way today.
    #[test]
    fn a_route_wrapped_onto_its_own_line_is_still_found() {
        let served = served_paths();
        assert!(
            served.contains(&"/api/indexer-node-health".to_string()),
            "multi-line route registrations are being missed: {served:?}"
        );
    }

    #[test]
    fn nothing_is_excused_twice_or_without_a_reason() {
        for (path, _, why) in UNCHECKED {
            assert!(!why.trim().is_empty(), "{path} is excused without a reason");
            assert_eq!(
                UNCHECKED.iter().filter(|(p, _, _)| p == path).count(),
                1,
                "{path} is listed twice"
            );
        }
    }

    /// An excuse for a route that no longer exists is a stale excuse, and it would silently cover
    /// for a future route that happened to take the same path.
    #[test]
    fn every_excuse_is_for_a_route_that_exists() {
        let served = served_paths();
        for (path, _, _) in UNCHECKED {
            assert!(
                served.contains(&path.to_string()),
                "{path} is excused from comparison but the router does not serve it"
            );
        }
    }
}
