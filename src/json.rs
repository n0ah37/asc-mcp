//! Shaping App Store Connect responses before they reach the agent.
//!
//! A JSON:API document carries a lot of machinery that costs context and tells
//! an agent nothing: every resource repeats a `self` link, and every one of its
//! twenty-odd relationships repeats two more. A single `list_apps` page can be
//! mostly links. Two passes fix that:
//!
//! 1. [`compact`] drops per-resource `links` and relationships that contain
//!    nothing but links. Nothing addressable by the API is lost — top-level
//!    `links` (which carry `next` for pagination) and `meta` are untouched.
//! 2. [`cap`] bounds the serialized size, shedding `included` first and then
//!    trailing `data` items, and says in the document what it dropped.
//!
//! Results are serialized **compactly**. Indented JSON measured 1.72× the bytes
//! of the same document compact — that is 72% more tokens for identical
//! information, and it means a given byte budget carries ~40% less of the data
//! the agent actually asked for. Models read compact JSON perfectly well.
//!
//! Compactness also makes the size arithmetic exact: in compact form a `data`
//! array costs the sum of its items plus one comma between each, so [`cap`] can
//! measure every item once and take a prefix, rather than repeatedly cloning and
//! re-serializing candidate documents to binary-search for the cut.
//!
//! Both passes are pure functions over [`serde_json::Value`], so the rules are
//! testable without a server or a network.

use std::io::Write;

use serde_json::{json, Map, Value};

use crate::config::OutputConfig;

/// Apply the configured compaction and size cap, then serialize.
pub fn render(value: Value, output: &OutputConfig) -> String {
    let value = if output.compact {
        compact(value)
    } else {
        value
    };
    let value = cap(value, output.max_bytes);
    serde_json::to_string(&value).unwrap_or_else(|_| value.to_string())
}

/// Strip redundant JSON:API navigation metadata from a response document.
pub fn compact(mut value: Value) -> Value {
    let Some(doc) = value.as_object_mut() else {
        return value;
    };

    if let Some(data) = doc.get_mut("data") {
        match data {
            Value::Array(items) => items.iter_mut().for_each(compact_resource),
            other => compact_resource(other),
        }
    }
    if let Some(Value::Array(included)) = doc.get_mut("included") {
        included.iter_mut().for_each(compact_resource);
    }
    value
}

/// Compact one resource object in place: `{type, id, attributes, relationships, links}`.
fn compact_resource(value: &mut Value) {
    let Some(resource) = value.as_object_mut() else {
        return;
    };
    resource.remove("links");

    let Some(Value::Object(relationships)) = resource.get_mut("relationships") else {
        return;
    };
    // A relationship that only has `links` is pure navigation: the agent can
    // reach the same resources through the documented endpoints.
    relationships.retain(|_, rel| {
        if let Some(obj) = rel.as_object_mut() {
            obj.remove("links");
            !obj.is_empty()
        } else {
            true
        }
    });
    if relationships.is_empty() {
        resource.remove("relationships");
    }
}

/// Bound the serialized size of a document to `max_bytes` (`0` disables).
///
/// Sheds `included` first — it is supporting detail the agent can re-fetch —
/// then keeps the longest prefix of `data` that fits, always at least one item.
/// The result is always valid JSON, and always says what went missing.
pub fn cap(value: Value, max_bytes: usize) -> Value {
    if max_bytes == 0 || serialized_len(&value) <= max_bytes {
        return value;
    }
    let Value::Object(mut doc) = value else {
        // Not a JSON:API document (Apple returns bare values from a few
        // endpoints); there is no safe structural trim, so pass it through.
        return value;
    };

    let mut note = overrun_note(max_bytes);

    if let Some(Value::Array(included)) = doc.get("included") {
        if !included.is_empty() {
            note.insert("includedDropped".into(), json!(included.len()));
            doc.remove("included");
        }
    }

    // Take `data` out of the document so trimming it is a truncation, never a
    // copy: the items we keep are never cloned, and the ones we drop are freed.
    let items = match doc.get("data") {
        Some(Value::Array(_)) => doc.remove("data").and_then(|d| match d {
            Value::Array(items) => Some(items),
            _ => None,
        }),
        _ => None,
    };
    // Fewer than two items leaves nothing to trim: a single resource can be
    // bigger than the whole budget on its own. Report the overrun rather than
    // hide it — but put `data` back first.
    let mut items = match items {
        Some(items) if items.len() > 1 => items,
        other => {
            if let Some(items) = other {
                doc.insert("data".into(), Value::Array(items));
            }
            note.insert("oversized".into(), json!(true));
            doc.insert("_truncated".into(), Value::Object(note));
            return Value::Object(doc);
        }
    };

    let total = items.len();
    // Seed the counts with their widest value (the total); the final numbers can
    // only be shorter, so a document that fits with these still fits with those.
    note.insert("dataItemsReturned".into(), json!(total));
    note.insert("dataItemsAvailable".into(), json!(total));
    doc.insert("_truncated".into(), Value::Object(note.clone()));
    doc.insert("data".into(), Value::Array(Vec::new()));

    // In compact JSON the document is exactly the empty-data document plus each
    // item plus one comma between them, so one measuring pass gives the answer.
    let mut used = serialized_len_of(&doc);
    let mut kept = 0usize;
    for (index, item) in items.iter().enumerate() {
        let cost = serialized_len(item) + usize::from(index > 0);
        if kept >= 1 && used + cost > max_bytes {
            break;
        }
        used += cost;
        kept += 1;
    }

    items.truncate(kept);
    if kept == total {
        // Nothing was dropped from `data`; don't imply otherwise.
        note.remove("dataItemsReturned");
        note.remove("dataItemsAvailable");
    } else {
        note.insert("dataItemsReturned".into(), json!(kept));
    }
    doc.insert("data".into(), Value::Array(items));
    if used > max_bytes {
        note.insert("oversized".into(), json!(true));
    }
    doc.insert("_truncated".into(), Value::Object(note));
    Value::Object(doc)
}

/// The explanation attached to any document that had to be trimmed.
fn overrun_note(max_bytes: usize) -> Map<String, Value> {
    let mut note = Map::new();
    note.insert(
        "reason".into(),
        json!(format!(
            "Response exceeded the {max_bytes}-byte tool-result budget (ASC_MAX_RESPONSE_BYTES)."
        )),
    );
    note.insert(
        "hint".into(),
        json!(
            "Re-request with a smaller `limit`, a narrower `filter[...]`, or `fields[...]` sparse \
fieldsets. Do NOT follow `links.next` to recover the dropped items — it points past this whole \
page, so they would be skipped."
        ),
    );
    note
}

/// An [`std::io::Write`] that keeps the byte count and throws the bytes away.
///
/// Measuring with `to_string(...).len()` allocates the whole document just to
/// read its length — several megabytes for a large page, discarded immediately.
struct CountingWriter(usize);

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serialized length of a value, without building the string.
fn serialized_len(value: &Value) -> usize {
    let mut counter = CountingWriter(0);
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.0,
        Err(_) => 0,
    }
}

/// Serialized length of a document body, without building the string.
fn serialized_len_of(doc: &Map<String, Value>) -> usize {
    let mut counter = CountingWriter(0);
    match serde_json::to_writer(&mut counter, doc) {
        Ok(()) => counter.0,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_page(count: usize) -> Value {
        let data: Vec<Value> = (0..count)
            .map(|i| {
                json!({
                    "type": "apps",
                    "id": format!("{i:08}"),
                    "attributes": { "name": format!("App number {i}"), "sku": format!("SKU{i}") },
                    "relationships": {
                        "builds": { "links": {
                            "self": "https://api.appstoreconnect.apple.com/v1/apps/x/relationships/builds",
                            "related": "https://api.appstoreconnect.apple.com/v1/apps/x/builds"
                        }},
                        "appInfos": { "links": {
                            "self": "https://api.appstoreconnect.apple.com/v1/apps/x/relationships/appInfos",
                            "related": "https://api.appstoreconnect.apple.com/v1/apps/x/appInfos"
                        }}
                    },
                    "links": { "self": "https://api.appstoreconnect.apple.com/v1/apps/x" }
                })
            })
            .collect();
        json!({
            "data": data,
            "links": { "self": "https://api.appstoreconnect.apple.com/v1/apps",
                       "next": "https://api.appstoreconnect.apple.com/v1/apps?cursor=NEXT" },
            "meta": { "paging": { "total": 500, "limit": count } }
        })
    }

    #[test]
    fn compact_drops_resource_links_and_link_only_relationships() {
        let out = compact(app_page(2));
        let first = &out["data"][0];
        assert!(first.get("links").is_none(), "resource self link kept");
        assert!(
            first.get("relationships").is_none(),
            "link-only relationships kept: {first}"
        );
        // Content survives untouched.
        assert_eq!(first["attributes"]["name"], "App number 0");
        assert_eq!(first["id"], "00000000");
    }

    #[test]
    fn compact_preserves_top_level_links_for_pagination() {
        let out = compact(app_page(1));
        assert_eq!(
            out["links"]["next"],
            "https://api.appstoreconnect.apple.com/v1/apps?cursor=NEXT"
        );
        assert_eq!(out["meta"]["paging"]["total"], 500);
    }

    #[test]
    fn compact_keeps_relationships_that_carry_data() {
        let doc = json!({
            "data": {
                "type": "appStoreVersions", "id": "v1",
                "relationships": {
                    "app": {
                        "data": { "type": "apps", "id": "a1" },
                        "links": { "self": "https://example.com/self" }
                    }
                }
            }
        });
        let out = compact(doc);
        assert_eq!(out["data"]["relationships"]["app"]["data"]["id"], "a1");
        assert!(out["data"]["relationships"]["app"].get("links").is_none());
    }

    #[test]
    fn compact_leaves_non_documents_alone() {
        assert_eq!(compact(json!(null)), json!(null));
        assert_eq!(compact(json!([1, 2])), json!([1, 2]));
    }

    #[test]
    fn measuring_agrees_with_serializing() {
        // The whole size arithmetic rests on this.
        for value in [
            app_page(0),
            app_page(1),
            app_page(37),
            json!(null),
            json!("x"),
        ] {
            assert_eq!(
                serialized_len(&value),
                serde_json::to_string(&value).unwrap().len(),
                "length mismatch for {value:.60}"
            );
        }
    }

    #[test]
    fn cap_of_zero_disables_the_budget() {
        let big = app_page(200);
        assert_eq!(cap(big.clone(), 0), big);
    }

    #[test]
    fn cap_leaves_small_documents_untouched() {
        let small = json!({ "data": { "type": "apps", "id": "1" } });
        assert_eq!(cap(small.clone(), 10_000), small);
    }

    #[test]
    fn cap_drops_included_before_trimming_data() {
        let doc = json!({
            "data": [{ "type": "apps", "id": "1" }],
            "included": (0..200).map(|i| json!({
                "type": "builds", "id": i.to_string(),
                "attributes": { "version": format!("build number {i} with padding text") }
            })).collect::<Vec<_>>(),
        });
        let out = cap(doc, 2_000);
        assert!(out.get("included").is_none(), "included survived the cap");
        assert_eq!(
            out["data"].as_array().unwrap().len(),
            1,
            "data was trimmed unnecessarily"
        );
        assert_eq!(out["_truncated"]["includedDropped"], 200);
    }

    #[test]
    fn cap_trims_data_items_and_reports_the_count() {
        let out = cap(compact(app_page(200)), 4_000);
        let kept = out["data"].as_array().unwrap().len();
        assert!((1..200).contains(&kept), "kept {kept} of 200");
        assert_eq!(out["_truncated"]["dataItemsReturned"], kept);
        assert_eq!(out["_truncated"]["dataItemsAvailable"], 200);
        // `links.next` points past this whole page, so following it after a trim
        // silently skips the dropped items. The note has to say so.
        assert!(out["_truncated"]["hint"]
            .as_str()
            .unwrap()
            .contains("Do NOT follow `links.next`"));
    }

    #[test]
    fn the_kept_prefix_is_the_largest_one_that_fits() {
        // Exactness, not just "under budget": adding the next item must overflow.
        let budget = 4_000;
        let page = compact(app_page(200));
        let all: Vec<Value> = page["data"].as_array().unwrap().clone();
        let out = cap(page, budget);
        let kept = out["data"].as_array().unwrap().len();

        assert!(serialized_len(&out) <= budget, "over budget");
        let mut one_more = out.clone();
        one_more["data"] = json!(all[..kept + 1].to_vec());
        assert!(
            serialized_len(&one_more) > budget,
            "kept {kept} but {} would also have fit",
            kept + 1
        );
    }

    #[test]
    fn capped_output_respects_the_budget_and_stays_valid_json() {
        for budget in [1_000, 4_000, 20_000, 60_000] {
            let out = cap(compact(app_page(200)), budget);
            let text = serde_json::to_string(&out).unwrap();
            assert!(
                text.len() <= budget,
                "budget {budget}, produced {}",
                text.len()
            );
            serde_json::from_str::<Value>(&text).expect("capped output must parse");
        }
    }

    #[test]
    fn cap_keeps_at_least_one_item_and_flags_the_overrun() {
        // One item alone blows the budget: keep it, but say so.
        let out = cap(app_page(50), 200);
        assert_eq!(out["data"].as_array().unwrap().len(), 1);
        assert_eq!(out["_truncated"]["dataItemsAvailable"], 50);
        assert_eq!(out["_truncated"]["oversized"], true);
    }

    #[test]
    fn cap_flags_a_single_oversized_resource_it_cannot_trim() {
        let doc = json!({ "data": { "type": "apps", "id": "1", "attributes": { "blob": "x".repeat(5_000) } } });
        let out = cap(doc, 1_000);
        assert_eq!(out["_truncated"]["oversized"], true);
        assert!(
            out["data"]["attributes"]["blob"].is_string(),
            "content must survive"
        );
    }

    #[test]
    fn cap_preserves_a_single_element_data_array() {
        let doc = json!({ "data": [{ "type": "apps", "id": "1", "attributes": { "blob": "x".repeat(5_000) } }] });
        let out = cap(doc, 1_000);
        assert_eq!(
            out["data"].as_array().unwrap().len(),
            1,
            "data went missing"
        );
        assert_eq!(out["_truncated"]["oversized"], true);
    }

    #[test]
    fn render_applies_both_passes() {
        let text = render(
            app_page(200),
            &OutputConfig {
                compact: true,
                max_bytes: 5_000,
            },
        );
        assert!(text.len() <= 5_000);
        assert!(
            !text.contains("relationships/builds"),
            "links survived compaction"
        );
        assert!(text.contains("_truncated"));
    }

    #[test]
    fn render_without_compaction_keeps_links() {
        let text = render(
            app_page(1),
            &OutputConfig {
                compact: false,
                max_bytes: 0,
            },
        );
        assert!(text.contains("relationships/builds"));
    }

    #[test]
    fn render_emits_compact_json() {
        // Indentation measured 1.72x the bytes for identical content.
        let text = render(
            app_page(3),
            &OutputConfig {
                compact: false,
                max_bytes: 0,
            },
        );
        assert!(!text.contains("\n  "), "output is indented: {text:.120}");
        serde_json::from_str::<Value>(&text).expect("must still parse");
    }
}
