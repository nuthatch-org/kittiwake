//! The same six templates through both implementations, compared field by field.
//!
//! `reference.json` was produced by lodestar's `src/lib/issue-form.ts` on 2026-09-11 over the YAML
//! beside it, which is what `nuthatch-org/graph-support` actually ships. Regenerating it needs the
//! Next repository, so it is committed rather than computed: a shape diff cannot see this, because
//! both sides return a string and only its content differs.
//!
//! What this pins is the part with no second chance. A thread filed with a heading GitHub would
//! not have written, or an unanswered optional rendered as an empty section rather than
//! `_No response_`, is a public issue in somebody else's repository that cannot be un-filed.

use kittiwake_support::{
    Answer, FieldType, IssueForm, Values, answerable, parse_issue_form, render_issue_body, validate,
};

const REFERENCE: &str = include_str!("fixtures/reference.json");

fn forms() -> Vec<(String, String)> {
    vec![
        (
            "01-query-error.yml",
            include_str!("fixtures/01-query-error.yml"),
        ),
        ("02-subgraph.yml", include_str!("fixtures/02-subgraph.yml")),
        ("03-indexer.yml", include_str!("fixtures/03-indexer.yml")),
        ("04-docs.yml", include_str!("fixtures/04-docs.yml")),
        ("05-question.yml", include_str!("fixtures/05-question.yml")),
        ("06-symptom.yml", include_str!("fixtures/06-symptom.yml")),
    ]
    .into_iter()
    .map(|(f, y)| (f.to_string(), y.to_string()))
    .collect()
}

/// The same deterministic answers the reference script used: every other checkbox, the first
/// dropdown option, and `answer for <id>` everywhere else.
fn answers(form: &IssueForm) -> Values {
    let mut v = std::collections::HashMap::new();
    for f in answerable(form) {
        let Some(id) = f.id.clone() else { continue };
        let a = match f.field_type {
            FieldType::Checkboxes => Answer::Many(
                f.checkboxes
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .step_by(2)
                    .map(|b| b.label.clone())
                    .collect(),
            ),
            FieldType::Dropdown => Answer::Many(
                f.options
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .take(1)
                    .cloned()
                    .collect(),
            ),
            _ => Answer::One(format!("answer for {id}")),
        };
        v.insert(id, a);
    }
    Values(v)
}

#[test]
fn every_template_parses_and_renders_as_the_incumbent_does() {
    let reference: serde_json::Value = serde_json::from_str(REFERENCE).unwrap();
    let mut checked = 0;

    for (file, yaml) in forms() {
        let want = &reference[&file];
        assert!(!want.is_null(), "{file} has no reference entry");

        let form = parse_issue_form(&file, &yaml).unwrap_or_else(|e| panic!("{file}: {e}"));

        assert_eq!(form.name, want["name"].as_str().unwrap(), "{file}: name");
        assert_eq!(
            form.description,
            want["description"].as_str().unwrap(),
            "{file}: description"
        );
        assert_eq!(
            form.title_prefix,
            want["titlePrefix"].as_str().unwrap(),
            "{file}: titlePrefix"
        );
        assert_eq!(
            serde_json::to_value(&form.labels).unwrap(),
            want["labels"],
            "{file}: labels"
        );

        let ids: Vec<serde_json::Value> = form
            .fields
            .iter()
            .map(|f| match &f.id {
                Some(i) => serde_json::Value::String(i.clone()),
                None => serde_json::Value::Null,
            })
            .collect();
        assert_eq!(
            serde_json::Value::Array(ids),
            want["fieldIds"],
            "{file}: field ids, including the positional ones"
        );

        let types: Vec<serde_json::Value> = form
            .fields
            .iter()
            .map(|f| serde_json::to_value(f.field_type).unwrap())
            .collect();
        assert_eq!(
            serde_json::Value::Array(types),
            want["fieldTypes"],
            "{file}: field types"
        );

        let required: Vec<serde_json::Value> =
            form.fields.iter().map(|f| f.required.into()).collect();
        assert_eq!(
            serde_json::Value::Array(required),
            want["required"],
            "{file}: which fields are required"
        );

        let filled = answers(&form);
        assert_eq!(
            serde_json::to_value(validate(&form, &filled)).unwrap(),
            want["errors"],
            "{file}: a filled form must validate the same way"
        );
        assert_eq!(
            render_issue_body(&form, &filled),
            want["body"].as_str().unwrap(),
            "{file}: the body GitHub would have composed"
        );

        // The empty case is the one that goes wrong quietly: an unanswered optional has to render
        // `_No response_` under its heading, not an empty section.
        let empty = Values::default();
        assert_eq!(
            render_issue_body(&form, &empty),
            want["emptyBody"].as_str().unwrap(),
            "{file}: an unanswered form"
        );
        assert_eq!(
            serde_json::to_value(validate(&form, &empty)).unwrap(),
            want["emptyErrors"],
            "{file}: what an unanswered form complains about"
        );

        checked += 1;
    }

    assert_eq!(checked, 6, "all six templates must have been compared");
}

/// A broken mirror must be an error, not an empty form that files a blank issue.
#[test]
fn yaml_that_is_not_an_issue_form_is_refused() {
    assert!(parse_issue_form("x.yml", "just a string").is_err());
    assert!(parse_issue_form("x.yml", "name: a form\nbody: []\n").is_err());
    assert!(parse_issue_form("x.yml", "- one\n- two\n").is_err());
    assert!(parse_issue_form("x.yml", ": : :").is_err());
}
