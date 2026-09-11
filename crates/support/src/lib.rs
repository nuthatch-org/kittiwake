//! GitHub issue forms, parsed from the YAML the repository actually ships.
//!
//! The templates are mirrored into Postgres verbatim by `kittiwake_ingest::graph_support`, so the
//! compose page renders whatever `nightswatchhq/graph-support` currently declares rather than a
//! copy that drifts from it. A field added there appears here on the mirror's next run and nothing
//! is edited.
//!
//! [`render_issue_body`] reproduces the markdown GitHub itself composes on submit - `### label`,
//! the value, `_No response_` for an unanswered optional - so a thread filed from here is
//! indistinguishable in the archive from one filed on GitHub.
//!
//! Ported from lodestar's `src/lib/issue-form.ts`.

use serde::Serialize;

pub const REPO: &str = "nightswatchhq/graph-support";

pub fn chooser_url() -> String {
    format!("https://github.com/{REPO}/issues/new/choose")
}

const NO_RESPONSE: &str = "_No response_";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    Input,
    Textarea,
    Dropdown,
    Checkboxes,
    /// Prose. Shown above the fields and collects nothing.
    Markdown,
}

impl FieldType {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "input" => Self::Input,
            "textarea" => Self::Textarea,
            "dropdown" => Self::Dropdown,
            "checkboxes" => Self::Checkboxes,
            "markdown" => Self::Markdown,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Checkbox {
    pub label: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueFormField {
    #[serde(rename = "type")]
    pub field_type: FieldType,
    /// Absent on `markdown` blocks, which carry prose and collect nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    pub required: bool,
    /// `markdown` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// `dropdown` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    /// `checkboxes` only, each with its own required flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkboxes: Option<Vec<Checkbox>>,
    /// `textarea` only: the language its value is fenced in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueForm {
    pub file: String,
    pub name: String,
    pub description: String,
    /// The seed GitHub puts in the title box, e.g. `[query] `.
    pub title_prefix: String,
    pub labels: Vec<String>,
    pub fields: Vec<IssueFormField>,
}

/// Answers keyed by field id. A checkbox group holds the labels that were ticked.
#[derive(Debug, Clone, Default)]
pub struct Values(pub std::collections::HashMap<String, Answer>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    One(String),
    Many(Vec<String>),
}

impl Values {
    fn get(&self, id: Option<&String>) -> Option<&Answer> {
        self.0.get(id?)
    }

    fn text(&self, id: Option<&String>) -> String {
        match self.get(id) {
            Some(Answer::One(s)) => s.trim().to_string(),
            _ => String::new(),
        }
    }

    fn list(&self, id: Option<&String>) -> Vec<String> {
        match self.get(id) {
            Some(Answer::Many(v)) => v.clone(),
            Some(Answer::One(s)) if !s.is_empty() => vec![s.clone()],
            _ => Vec::new(),
        }
    }
}

fn str_of(v: Option<&serde_yaml_ng::Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(str::to_string)
}

fn parse_field(raw: &serde_yaml_ng::Value, index: usize) -> Option<IssueFormField> {
    let field_type = FieldType::parse(raw.get("type")?.as_str()?)?;
    let attrs = raw.get("attributes");
    let attr = |k: &str| attrs.and_then(|a| a.get(k));

    let required = raw
        .get("validations")
        .and_then(|v| v.get("required"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // A collecting field with no id cannot be answered or rendered back, so it is given a stable
    // positional one rather than dropped: dropping it would silently lose a question.
    let id = str_of(raw.get("id")).or(match field_type {
        FieldType::Markdown => None,
        _ => Some(format!("field_{index}")),
    });

    let mut field = IssueFormField {
        field_type,
        id,
        label: str_of(attr("label")),
        description: str_of(attr("description")),
        placeholder: str_of(attr("placeholder")),
        required,
        value: str_of(attr("value")),
        options: None,
        multiple: None,
        checkboxes: None,
        render: str_of(attr("render")),
    };

    if field_type == FieldType::Dropdown {
        field.options = Some(
            attr("options")
                .and_then(|o| o.as_sequence())
                .map(|s| s.iter().map(yaml_to_string).collect())
                .unwrap_or_default(),
        );
        field.multiple = Some(attr("multiple").and_then(|m| m.as_bool()).unwrap_or(false));
    }

    if field_type == FieldType::Checkboxes {
        field.checkboxes = Some(
            attr("options")
                .and_then(|o| o.as_sequence())
                .map(|s| {
                    s.iter()
                        .filter_map(|b| {
                            Some(Checkbox {
                                label: str_of(b.get("label"))?,
                                required: b
                                    .get("required")
                                    .and_then(|r| r.as_bool())
                                    .unwrap_or(false),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        );
    }

    Some(field)
}

/// YAML's `String(..)` for a string and its own rendering for anything else, matching the
/// incumbent's `String(v)`. A template that writes an option as a bare number stays usable.
fn yaml_to_string(v: &serde_yaml_ng::Value) -> String {
    match v {
        serde_yaml_ng::Value::String(s) => s.clone(),
        serde_yaml_ng::Value::Number(n) => n.to_string(),
        serde_yaml_ng::Value::Bool(b) => b.to_string(),
        other => serde_yaml_ng::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

/// An error on YAML that is not an issue form, which is the only useful answer for a broken mirror.
pub fn parse_issue_form(file: &str, yaml: &str) -> Result<IssueForm, String> {
    let doc: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(yaml).map_err(|e| format!("{file}: {e}"))?;
    if !doc.is_mapping() {
        return Err(format!("{file}: not a YAML mapping"));
    }

    let fields: Vec<IssueFormField> = doc
        .get("body")
        .and_then(|b| b.as_sequence())
        .map(|s| {
            s.iter()
                .enumerate()
                .filter_map(|(i, f)| parse_field(f, i))
                .collect()
        })
        .unwrap_or_default();
    if fields.is_empty() {
        return Err(format!("{file}: no fields"));
    }

    Ok(IssueForm {
        file: file.to_string(),
        name: str_of(doc.get("name")).unwrap_or_else(|| file.to_string()),
        description: str_of(doc.get("description")).unwrap_or_default(),
        title_prefix: str_of(doc.get("title")).unwrap_or_default(),
        labels: doc
            .get("labels")
            .and_then(|l| l.as_sequence())
            .map(|s| s.iter().map(yaml_to_string).collect())
            .unwrap_or_default(),
        fields,
    })
}

/// The fields a reporter answers, in order.
pub fn answerable(form: &IssueForm) -> impl Iterator<Item = &IssueFormField> {
    form.fields
        .iter()
        .filter(|f| f.field_type != FieldType::Markdown && f.id.is_some())
}

/// Empty strings count as unanswered; a required field answered with spaces is not answered.
pub fn validate(form: &IssueForm, values: &Values) -> Vec<String> {
    let mut errors = Vec::new();

    for field in answerable(form) {
        let label = field
            .label
            .clone()
            .or_else(|| field.id.clone())
            .unwrap_or_else(|| "a field".into());

        match field.field_type {
            FieldType::Checkboxes => {
                let ticked = values.list(field.id.as_ref());
                for box_ in field.checkboxes.as_deref().unwrap_or_default() {
                    if box_.required && !ticked.contains(&box_.label) {
                        errors.push(format!("\"{}\" must be ticked", box_.label));
                    }
                }
            }
            FieldType::Dropdown => {
                let chosen = values.list(field.id.as_ref());
                let options = field.options.as_deref().unwrap_or_default();
                if let Some(unknown) = chosen.iter().find(|c| !options.contains(c)) {
                    errors.push(format!("{label}: \"{unknown}\" is not one of the options"));
                }
                if field.required && chosen.is_empty() {
                    errors.push(format!("{label} is required"));
                }
            }
            _ => {
                if field.required && values.text(field.id.as_ref()).is_empty() {
                    errors.push(format!("{label} is required"));
                }
            }
        }
    }

    errors
}

fn render_value(field: &IssueFormField, values: &Values) -> String {
    match field.field_type {
        FieldType::Checkboxes => {
            let ticked = values.list(field.id.as_ref());
            field
                .checkboxes
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|b| {
                    format!(
                        "- [{}] {}",
                        if ticked.contains(&b.label) { "x" } else { " " },
                        b.label
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        FieldType::Dropdown => {
            let chosen = values.list(field.id.as_ref());
            if chosen.is_empty() {
                NO_RESPONSE.to_string()
            } else {
                chosen.join(", ")
            }
        }
        _ => {
            let text = values.text(field.id.as_ref());
            if text.is_empty() {
                NO_RESPONSE.to_string()
            } else if let Some(lang) = &field.render {
                // `render` fences the answer in the language the template declared, which is what
                // makes a JSON error or a GraphQL query readable in the thread rather than one
                // reflowed paragraph.
                format!("```{lang}\n{text}\n```")
            } else {
                text
            }
        }
    }
}

/// The markdown GitHub would have composed for the same answers.
pub fn render_issue_body(form: &IssueForm, values: &Values) -> String {
    answerable(form)
        .map(|f| {
            let heading = f.label.clone().or_else(|| f.id.clone()).unwrap_or_default();
            format!("### {}\n\n{}", heading, render_value(f, values))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The provenance line every issue filed from here carries.
///
/// It exists because the credential is one account: without this the archive would show every
/// report as opened by the maintainer, and that repository credits reporters by name and answers
/// them in the thread. A handle is optional, and saying nobody left one is better than implying
/// the account is the reporter.
pub fn provenance_footer(handle: Option<&str>) -> String {
    match handle.map(str::trim).filter(|h| !h.is_empty()) {
        Some(who) => format!(
            "---\n\nFiled through [Lodestar](https://www.lodestar-dashboard.com/support) on behalf \
             of {who}, who is not the account that opened this thread."
        ),
        None => "---\n\nFiled through [Lodestar](https://www.lodestar-dashboard.com/support). The \
                 reporter left no handle, so the account that opened this thread is not theirs."
            .to_string(),
    }
}
