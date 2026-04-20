use criterion::{Criterion, black_box, criterion_group, criterion_main};
use madcap_fast::merge_track_pages;
use serde_json::{Value, json};

/// Build `pages` pages of fake tracks with monotonically-increasing timestamps
/// across pages (page 0 = newest). Each participant's per-page slice is already
/// ordered oldest-to-newest within the page, matching the upstream.
fn make_pages(pages: usize, participants: usize, pts_per_page: usize) -> Vec<Value> {
    let mut out = Vec::with_capacity(pages);
    for page_idx in 0..pages {
        let age = pages - 1 - page_idx;
        let start_ts = (age * pts_per_page) as f64;
        let mut tracks = Vec::with_capacity(participants);
        for pid in 0..participants {
            let track: Vec<Value> = (0..pts_per_page)
                .map(|i| {
                    let t = start_ts + i as f64;
                    json!([
                        t,
                        43.0 + (i as f64) * 0.0001,
                        -1.0 - (i as f64) * 0.0001,
                        100.0,
                        10.0,
                        80
                    ])
                })
                .collect();
            tracks.push(json!({
                "participant_id": format!("p{:04}", pid),
                "track": track,
            }));
        }
        out.push(json!({
            "tracks": tracks,
            "previous_page_ts": if page_idx + 1 < pages { Some((pages - page_idx - 2) as f64) } else { None },
            "current_page_last_ts": (age * pts_per_page + pts_per_page - 1) as f64,
        }));
    }
    out
}

fn bench_merge(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge_track_pages");

    let small = make_pages(3, 50, 100);
    group.bench_function("3p_50x100", |b| {
        b.iter(|| merge_track_pages(black_box(&small)))
    });

    let realistic = make_pages(3, 320, 200);
    group.bench_function("3p_320x200 (desertus today)", |b| {
        b.iter(|| merge_track_pages(black_box(&realistic)))
    });

    let worst = make_pages(10, 320, 200);
    group.bench_function("10p_320x200 (~10d event)", |b| {
        b.iter(|| merge_track_pages(black_box(&worst)))
    });

    group.finish();
}

criterion_group!(benches, bench_merge);
criterion_main!(benches);
