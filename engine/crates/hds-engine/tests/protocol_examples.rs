//! Every JSON example of docs/PROTOCOL.md, read at test time, parses into
//! the engine's types and serializes back to the same JSON (member order
//! aside), so every documented member exists, with the documented
//! nullability, and no type has a member the document does not show. A new
//! example that no type is mapped to fails the test.
//!
//! Placeholders in the document are expanded before parsing: a shared
//! object's name (`Geometry`, `[Partition, ...]`, `Row + {...}`) becomes the
//! example of that object, `...` is dropped, `"a"|"b"` yields one example
//! per alternative. `{"a", "b"}` is a list of member names and is compared
//! with the members of the type.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use hds_engine::protocol::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};

/// One example: the heading it is under, its form, its text.
#[derive(Debug, Clone)]
struct Example {
    heading: String,
    block: bool,
    text: String,
}

/// Fenced blocks and inline code spans that start with `{` or `[`.
fn extract(markdown: &str) -> Vec<Example> {
    let mut out = Vec::new();
    let mut heading = String::new();
    let mut lines = markdown.lines();
    while let Some(line) = lines.next() {
        if line.starts_with('#') {
            heading = line.trim_start_matches('#').trim().to_string();
            continue;
        }
        if let Some(lang) = line.strip_prefix("```") {
            let mut body = Vec::new();
            for inner in lines.by_ref() {
                if inner.starts_with("```") {
                    break;
                }
                body.push(inner);
            }
            if lang.trim() == "json" || body.iter().any(|l| l.contains('{')) {
                out.push(Example {
                    heading: heading.clone(),
                    block: true,
                    text: body.join("\n"),
                });
            }
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('`') else { break };
            let span = &after[..end];
            if span.starts_with('{') || span.starts_with('[') {
                out.push(Example {
                    heading: heading.clone(),
                    block: false,
                    text: span.to_string(),
                });
            }
            rest = &after[end + 1..];
        }
    }
    out
}

/// Expands `"a"|"b"` into one text per alternative (first group first).
fn alternatives(text: &str) -> Vec<String> {
    let Some(bar) = text.find("\"|\"") else {
        return vec![text.to_string()];
    };
    let open = text[..bar]
        .rfind('"')
        .expect("alternation starts with a string");
    let mut items = Vec::new();
    let mut pos = open;
    loop {
        let close = pos + 1 + text[pos + 1..].find('"').expect("string ends");
        items.push(text[pos..=close].to_string());
        if text[close + 1..].starts_with("|\"") {
            pos = close + 2;
        } else {
            pos = close + 1;
            break;
        }
    }
    items
        .iter()
        .flat_map(|item| alternatives(&format!("{}{}{}", &text[..open], item, &text[pos..])))
        .collect()
}

/// Replaces object names by their examples and drops `...`.
fn expand(text: &str, objects: &BTreeMap<String, String>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
        } else if chars[i..].starts_with(&['.', '.', '.']) {
            while out.ends_with(char::is_whitespace) {
                out.pop();
            }
            if out.ends_with(',') {
                out.pop();
            }
            i += 3;
        } else if c.is_ascii_uppercase() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphanumeric() {
                i += 1;
            }
            let name: String = chars[start..i].iter().collect();
            let object = objects
                .get(&name)
                .unwrap_or_else(|| panic!("unknown placeholder {name} in {text}"));
            let tail: String = chars[i..].iter().collect();
            if let Some(after) = tail.trim_start().strip_prefix("+ {") {
                // `Row + {...}`: the object's members followed by the extra ones.
                let body = object.trim_end().strip_suffix('}').expect("object");
                out.push_str(body);
                out.push_str(", ");
                i = chars.len() - after.chars().count();
            } else {
                out.push_str(object);
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn parse_json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("example does not parse ({e}):\n{text}"))
}

/// Parses into `T` and back.
fn round_trip<T: Serialize + DeserializeOwned>(value: &Value) -> Result<Value, String> {
    let typed: T = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    serde_json::to_value(&typed).map_err(|e| e.to_string())
}

/// How an example is checked.
#[derive(Clone, Copy)]
enum Check {
    /// Parses into a type and serializes back to the same value.
    Exact(fn(&Value) -> Result<Value, String>),
    /// A list of member names: the type's members.
    Keys(fn() -> Value),
    /// The transport block of section 1.
    Transport,
}

fn exact<T: Serialize + DeserializeOwned>() -> Check {
    Check::Exact(round_trip::<T>)
}

/// Which type each example is, by heading and position under it.
fn mapping() -> Vec<(&'static str, Vec<Check>)> {
    vec![
        ("1. Transport", vec![Check::Transport]),
        ("SourceInfo", vec![exact::<SourceInfo>()]),
        ("Partition", vec![exact::<Partition>()]),
        ("Disk", vec![exact::<Disk>()]),
        ("Warning", vec![exact::<Warning>()]),
        ("Geometry", vec![exact::<Geometry>()]),
        ("Instance", vec![exact::<Instance>()]),
        ("CatalogInfo", vec![exact::<CatalogInfo>()]),
        ("Row", vec![exact::<Row>()]),
        ("Filter", vec![exact::<Filter>()]),
        ("hello", vec![exact::<HelloResult>()]),
        ("list_disks", vec![exact::<ListDisksResult>()]),
        (
            "open_source",
            vec![exact::<OpenSourceArgs>(), exact::<OpenSourceResult>()],
        ),
        ("close_source", vec![exact::<Empty>()]),
        (
            "scan_start",
            vec![
                exact::<ScanStartArgs>(),
                exact::<ScanStartResult>(),
                exact::<ScanProgress>(),
                exact::<ScanDone>(),
            ],
        ),
        ("job_cancel", vec![exact::<Empty>(), exact::<Empty>()]),
        (
            "session_save",
            vec![exact::<SessionSaveArgs>(), exact::<SessionSaveResult>()],
        ),
        (
            "session_load",
            vec![
                exact::<SessionLoadArgs>(),
                Check::Keys(|| {
                    serde_json::to_value(SourceRef {
                        kind: SourceKind::Image,
                        path: String::new(),
                    })
                    .unwrap()
                }),
                exact::<SessionLoadResult>(),
            ],
        ),
        (
            "session_attach_source",
            vec![exact::<SessionAttachArgs>(), exact::<SessionAttachResult>()],
        ),
        ("geometry_set", vec![exact::<GeometrySetArgs>()]),
        ("instance_set", vec![exact::<InstanceSetArgs>()]),
        (
            "list_children",
            vec![exact::<ListChildrenArgs>(), exact::<RowsPage>()],
        ),
        (
            "node_info",
            vec![exact::<NodeInfoArgs>(), exact::<NodeInfo>()],
        ),
        ("search", vec![exact::<SearchArgs>(), exact::<SearchPage>()]),
        (
            "selection_summary",
            vec![exact::<SelectionArgs>(), exact::<SelectionSummary>()],
        ),
        (
            "check_destination",
            vec![
                exact::<CheckDestinationArgs>(),
                exact::<CheckDestinationResult>(),
            ],
        ),
        (
            "recover_start",
            vec![
                exact::<RecoverStartArgs>(),
                exact::<RecoverStartResult>(),
                exact::<RecoverProgress>(),
                exact::<RecoverDone>(),
            ],
        ),
        (
            "carve_start",
            vec![
                exact::<CarveStartArgs>(),
                exact::<CarveStartResult>(),
                exact::<CarveProgress>(),
                exact::<CarveDone>(),
            ],
        ),
        (
            "image_start",
            vec![
                exact::<ImageStartArgs>(),
                exact::<ImageStartResult>(),
                Check::Keys(|| serde_json::to_value(ImageProgress::default()).unwrap()),
                Check::Keys(|| serde_json::to_value(ImageDone::default()).unwrap()),
            ],
        ),
        (
            "read_file",
            vec![exact::<ReadFileArgs>(), exact::<ReadFileResult>()],
        ),
        ("shutdown", vec![exact::<Empty>()]),
        (
            "4. Other events",
            vec![exact::<TaskProgress>(), exact::<LogEvent>()],
        ),
    ]
}

/// `{"a", "b"}` as a set of names, if the text is such a list.
fn key_list(text: &str) -> Option<BTreeSet<String>> {
    let inner = text.trim().strip_prefix('{')?.strip_suffix('}')?;
    inner
        .split(',')
        .map(|item| {
            let item = item.trim();
            (item.len() >= 2 && item.starts_with('"') && item.ends_with('"') && !item.contains(':'))
                .then(|| item[1..item.len() - 1].to_string())
        })
        .collect()
}

fn check_transport(text: &str, objects: &BTreeMap<String, String>) -> usize {
    let mut checked = 0;
    for line in text.lines() {
        let Some(brace) = line.find('{') else {
            continue;
        };
        let label = line[..brace].trim();
        let json = expand(&line[brace..], objects);
        let value = parse_json(&json);
        if label == "request" {
            let request = parse_request(&json).expect("the request example is a valid request");
            let command = Command::from_name(&request.cmd).expect("a known command");
            assert_eq!(command, Command::ListChildren);
            let args: ListChildrenArgs =
                serde_json::from_value(Value::Object(request.args)).expect("its args parse");
            assert_eq!((args.node, args.epoch), (12, 3));
        } else if value.get("event").is_some() {
            assert_eq!(round_trip::<EventLine>(&value).unwrap(), value);
        } else {
            assert_eq!(round_trip::<Response>(&value).unwrap(), value, "{json}");
        }
        checked += 1;
    }
    assert_eq!(
        checked, 4,
        "the transport block shows a request, two responses and an event"
    );
    checked
}

#[test]
fn every_example_round_trips() {
    let markdown =
        std::fs::read_to_string(common::docs_dir().join("PROTOCOL.md")).expect("PROTOCOL.md");
    let examples = extract(&markdown);
    assert!(
        examples.len() > 40,
        "suspiciously few examples: {}",
        examples.len()
    );

    // The shared objects of section 2, expanded in document order.
    let mut objects: BTreeMap<String, String> = BTreeMap::new();
    for example in &examples {
        let name = example.heading.as_str();
        let is_object = [
            "SourceInfo",
            "Partition",
            "Disk",
            "Warning",
            "Geometry",
            "Instance",
            "CatalogInfo",
            "Row",
            "Filter",
        ]
        .contains(&name);
        if is_object && !objects.contains_key(name) {
            let mut text = expand(&example.text, &objects);
            if name == "Disk" {
                // "SourceInfo members plus": merge them in.
                let mut merged: Map<String, Value> = parse_json(&objects["SourceInfo"])
                    .as_object()
                    .unwrap()
                    .clone();
                merged.extend(parse_json(&text).as_object().unwrap().clone());
                text = Value::Object(merged).to_string();
            }
            objects.insert(name.to_string(), text);
        }
    }

    let mapping = mapping();
    let mut by_heading: BTreeMap<&str, Vec<&Example>> = BTreeMap::new();
    for example in &examples {
        by_heading
            .entry(example.heading.as_str())
            .or_default()
            .push(example);
    }
    let mut checked = 0;
    for (heading, found) in &by_heading {
        let checks = mapping
            .iter()
            .find(|(h, _)| h == heading)
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| {
                panic!("examples under \"{heading}\" are not mapped to types: {found:?}")
            });
        assert_eq!(
            checks.len(),
            found.len(),
            "\"{heading}\" has {} examples, {} are mapped: {found:?}",
            found.len(),
            checks.len()
        );
        for (example, check) in found.iter().zip(checks) {
            match check {
                Check::Transport => {
                    assert!(example.block);
                    checked += check_transport(&example.text, &objects);
                }
                Check::Keys(sample) => {
                    let keys = key_list(&example.text)
                        .unwrap_or_else(|| panic!("not a list of member names: {}", example.text));
                    let members: BTreeSet<String> = sample()
                        .as_object()
                        .expect("an object")
                        .keys()
                        .cloned()
                        .collect();
                    assert_eq!(keys, members, "members under \"{heading}\"");
                    checked += 1;
                }
                Check::Exact(round) => {
                    // A shared object is checked in its expanded (for Disk: merged) form.
                    let text = objects
                        .get(*heading)
                        .cloned()
                        .unwrap_or_else(|| example.text.clone());
                    for variant in alternatives(&text) {
                        let json = expand(&variant, &objects);
                        let value = parse_json(&json);
                        let back = round(&value)
                            .unwrap_or_else(|e| panic!("under \"{heading}\": {e}\n{json}"));
                        assert_eq!(
                            back, value,
                            "under \"{heading}\" the example does not round-trip"
                        );
                        checked += 1;
                    }
                }
            }
        }
    }
    for (heading, _) in &mapping {
        assert!(
            by_heading.contains_key(heading),
            "no example under \"{heading}\" any more"
        );
    }
    assert!(checked >= 60, "only {checked} examples checked");
}

/// The section-3 command headings are exactly the engine's commands.
#[test]
fn commands_match_the_document() {
    let markdown =
        std::fs::read_to_string(common::docs_dir().join("PROTOCOL.md")).expect("PROTOCOL.md");
    let section = markdown
        .split("## 3. Commands")
        .nth(1)
        .and_then(|s| s.split("## 4.").next())
        .expect("section 3");
    let documented: BTreeSet<&str> = section
        .lines()
        .filter_map(|l| l.strip_prefix("### "))
        .map(str::trim)
        .collect();
    let ours: BTreeSet<&str> = Command::ALL.iter().map(|c| c.name()).collect();
    assert_eq!(documented, ours);
}

/// Back-quoted words of the paragraph that follows `marker`.
fn listed_after<'a>(markdown: &'a str, marker: &str) -> BTreeSet<&'a str> {
    let start = markdown
        .find(marker)
        .unwrap_or_else(|| panic!("{marker} not found"))
        + marker.len();
    // Up to the end of the paragraph or of the list item.
    let paragraph = markdown[start..]
        .split("\n\n")
        .next()
        .and_then(|p| p.split("\n- ").next())
        .unwrap_or("");
    paragraph
        .split('`')
        .skip(1)
        .step_by(2)
        .map(|w| w.trim_matches('"'))
        .filter(|w| w.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        .collect()
}

fn wire_names<T: Serialize>(items: &[T]) -> BTreeSet<String> {
    items
        .iter()
        .map(|i| {
            serde_json::to_value(i)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn enumerations_match_the_document() {
    let markdown =
        std::fs::read_to_string(common::docs_dir().join("PROTOCOL.md")).expect("PROTOCOL.md");
    let codes: BTreeSet<String> = listed_after(&markdown, "### Error codes\n\n")
        .into_iter()
        .map(String::from)
        .collect();
    let ours = wire_names(&[
        ErrorCode::BadRequest,
        ErrorCode::UnknownCommand,
        ErrorCode::Busy,
        ErrorCode::NoSource,
        ErrorCode::NoSession,
        ErrorCode::NoCatalog,
        ErrorCode::StaleCatalog,
        ErrorCode::NotFound,
        ErrorCode::IoError,
        ErrorCode::AccessDenied,
        ErrorCode::DestinationRefused,
        ErrorCode::SourceMismatch,
        ErrorCode::SourceLost,
        ErrorCode::NoSpace,
        ErrorCode::Exists,
        ErrorCode::Unsupported,
        ErrorCode::Cancelled,
        ErrorCode::Internal,
    ]);
    assert_eq!(codes, ours);
    for code in &ours {
        let parsed: ErrorCode = serde_json::from_value(Value::String(code.clone())).unwrap();
        assert_eq!(parsed.as_str(), code);
    }

    let flags: BTreeSet<String> = listed_after(&markdown, "- `flags`: subset of")
        .into_iter()
        .map(String::from)
        .collect();
    let ours = wire_names(&[
        Flag::Deleted,
        Flag::TornRecord,
        Flag::MissingFragments,
        Flag::SizeEstimated,
        Flag::OutOfBounds,
        Flag::Overwritten,
        Flag::Overlap,
        Flag::Compressed,
        Flag::Encrypted,
        Flag::Placeholder,
        Flag::Sparse,
        Flag::NoData,
        Flag::BadSectors,
        Flag::OlderGeneration,
        Flag::Embedded,
        Flag::Stray,
        Flag::SignatureOk,
        Flag::SignatureMismatch,
    ]);
    assert_eq!(flags, ours);

    let kinds: BTreeSet<String> = listed_after(&markdown, "- `kind`: ")
        .into_iter()
        .map(String::from)
        .collect();
    let ours = wire_names(&[
        NodeKind::Dir,
        NodeKind::File,
        NodeKind::LostDir,
        NodeKind::Virtual,
        NodeKind::Carved,
    ]);
    assert_eq!(kinds, ours);
}

#[test]
fn placeholder_expansion() {
    let mut objects = BTreeMap::new();
    objects.insert("Row".to_string(), r#"{"id": 1}"#.to_string());
    assert_eq!(
        parse_json(&expand(
            r#"{"rows": [Row + {"path": "a"}, ...], "x": { ... }, "y": ["a", ...]}"#,
            &objects
        )),
        serde_json::json!({"rows": [{"id": 1, "path": "a"}], "x": {}, "y": ["a"]})
    );
    assert_eq!(
        alternatives(r#"{"k": "a"|"b"|"c", "s": "x|y"}"#),
        vec![
            r#"{"k": "a", "s": "x|y"}"#.to_string(),
            r#"{"k": "b", "s": "x|y"}"#.to_string(),
            r#"{"k": "c", "s": "x|y"}"#.to_string()
        ]
    );
    assert_eq!(
        key_list(r#"{"a", "b"}"#),
        Some(["a".to_string(), "b".to_string()].into_iter().collect())
    );
    assert_eq!(key_list(r#"{"a": 1}"#), None);
}
