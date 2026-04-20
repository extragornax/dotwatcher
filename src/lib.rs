use std::collections::HashMap;

use serde_json::Value;

/// Merge paginated tracks pages (as returned by the upstream) into a single
/// `{"tracks": [{"participant_id": ..., "track": [...]}]}` value.
///
/// Pages are expected newest-first (index 0 = newest, index N-1 = oldest),
/// matching the order in which they are walked via `previous_page_ts`.
/// Points within a participant are sorted by their first-element timestamp
/// and deduped at page boundaries.
pub fn merge_track_pages(pages: &[Value]) -> Value {
    let mut merged: HashMap<String, Vec<Value>> = HashMap::new();
    for page in pages.iter().rev() {
        let Some(tracks) = page.get("tracks").and_then(|t| t.as_array()) else {
            continue;
        };
        for t in tracks {
            let Some(pid) = t.get("participant_id").and_then(|p| p.as_str()) else {
                continue;
            };
            let Some(points) = t.get("track").and_then(|p| p.as_array()) else {
                continue;
            };
            merged
                .entry(pid.to_string())
                .or_default()
                .extend(points.iter().cloned());
        }
    }

    for track in merged.values_mut() {
        track.sort_by(|a, b| {
            let ta = a.get(0).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let tb = b.get(0).and_then(|v| v.as_f64()).unwrap_or(0.0);
            ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal)
        });
        track.dedup_by(|a, b| {
            let ta = a.get(0).and_then(|v| v.as_f64());
            let tb = b.get(0).and_then(|v| v.as_f64());
            ta.is_some() && ta == tb
        });
    }

    let tracks_array: Vec<Value> = merged
        .into_iter()
        .map(|(pid, track)| serde_json::json!({ "participant_id": pid, "track": track }))
        .collect();
    serde_json::json!({ "tracks": tracks_array })
}
