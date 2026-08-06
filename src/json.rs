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
//! Both are pure functions over [`serde_json::Value`], so the rules are testable
//! without a server or a network.

use serde_json::{json, Map, Value};

use crate::config::OutputConfig;

/// Apply the configured compaction and size cap, then pretty-print.
pub fn render(value: Value, output: &OutputConfig) -> String {
    let value = if output.compact {
        compact(value)
    } else {
        value
    };
    let value = cap(value, output.max_bytes);
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
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

/// Bound the pretty-printed size of a document to `max_bytes` (`0` disables).
///
/// Sheds `included` first — it is supporting detail the agent can re-fetch —
/// then trims `data` items from the end, keeping at least one. The result is
/// always valid JSON, and always says what went missing.
pub fn cap(value: Value, max_bytes: usize) -> Value {
    if max_bytes == 0 || rendered_len(&value) <= max_bytes {
        return value;
    }
    let Value::Object(mut doc) = value else {
        // Not a JSON:API document (Apple returns bare values from a few
        // endpoints); there is no safe structural trim, so pass it through.
        return value;
    };

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
            "Narrow the result with `limit`, a `filter[...]`, or `fields[...]` sparse fieldsets, \
or page through it with `cursor`."
        ),
    );

    if let Some(Value::Array(included)) = doc.get("included") {
        if !included.is_empty() {
            note.insert("includedDropped".into(), json!(included.len()));
            doc.remove("included");
        }
    }

    // The note is part of the payload, so it has to be inside the budget while
    // we search. Seed the counts with their widest value (the total) — the
    // final numbers can only be shorter, never longer.
    let total = doc.get("data").and_then(Value::as_array).map(Vec::len);
    if let Some(total) = total {
        note.insert("dataItemsReturned".into(), json!(total));
        note.insert("dataItemsAvailable".into(), json!(total));
    }
    doc.insert("_truncated".into(), Value::Object(note.clone()));

    let mut trimmed = false;
    if rendered_len_of(&doc) > max_bytes {
        if let (Some(total), Some(Value::Array(items))) = (total, doc.get("data").cloned()) {
            if total > 1 {
                let kept = largest_prefix_that_fits(&doc, &items, max_bytes);
                doc.insert("data".into(), Value::Array(items[..kept].to_vec()));
                note.insert("dataItemsReturned".into(), json!(kept));
                trimmed = true;
            }
        }
    }
    if !trimmed {
        // Nothing was dropped from `data`; don't imply otherwise.
        note.remove("dataItemsReturned");
        note.remove("dataItemsAvailable");
    }
    doc.insert("_truncated".into(), Value::Object(note.clone()));

    if rendered_len_of(&doc) > max_bytes {
        // A single resource can be bigger than the whole budget; there is no
        // safe structural trim left, so report the overrun rather than hide it.
        note.insert("oversized".into(), json!(true));
        doc.insert("_truncated".into(), Value::Object(note));
    }
    Value::Object(doc)
}

/// Binary-search the longest prefix of `items` whose document still fits.
/// Always returns at least 1 so the agent gets a usable sample.
fn largest_prefix_that_fits(doc: &Map<String, Value>, items: &[Value], max_bytes: usize) -> usize {
    let mut probe = doc.clone();
    let (mut low, mut high) = (1usize, items.len());
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        probe.insert("data".into(), Value::Array(items[..mid].to_vec()));
        if rendered_len_of(&probe) <= max_bytes {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

fn rendered_len(value: &Value) -> usize {
    serde_json::to_string_pretty(value)
        .map(|s| s.len())
        .unwrap_or(0)
}

fn rendered_len_of(doc: &Map<String, Value>) -> usize {
    serde_json::to_string_pretty(doc)
        .map(|s| s.len())
        .unwrap_or(0)
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
        assert!(out["_truncated"]["hint"]
            .as_str()
            .unwrap()
            .contains("cursor"));
    }

    #[test]
    fn capped_output_respects_the_budget_and_stays_valid_json() {
        let budget = 4_000;
        let out = cap(compact(app_page(200)), budget);
        let text = serde_json::to_string_pretty(&out).unwrap();
        assert!(
            text.len() <= budget,
            "capped to {} bytes, budget {budget}",
            text.len()
        );
        serde_json::from_str::<Value>(&text).expect("capped output must parse");
    }

    #[test]
    fn cap_keeps_at_least_one_item_and_flags_the_overrun() {
        // One item alone blows the budget: keep it, but say so.
        let out = cap(app_page(50), 200);
        assert_eq!(out["data"].as_array().unwrap().len(), 1);
        assert!(out["_truncated"]["dataItemsAvailable"] == 50);
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
}
