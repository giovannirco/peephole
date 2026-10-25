use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use chrono::{Duration, Utc};
use peephole_core::{Config, Store, Value, Widget};
use peephole_render::{RenderCtx, render_page};

const YAML: &str = include_str!("fixtures/sample.yaml");
const JSON: &str = include_str!("fixtures/sample.json");

fn store_from_json(text: &str) -> Store {
    let store = Store::new();
    let serde_json::Value::Object(map) = serde_json::from_str(text).expect("values JSON parses") else {
        panic!("values JSON must be an object");
    };
    for (key, v) in map {
        let value = match v {
            serde_json::Value::Number(n) => Value::Number(n.as_f64().expect("finite number")),
            serde_json::Value::String(s) => Value::Text(s),
            serde_json::Value::Array(a) => {
                Value::Series(a.iter().map(|x| x.as_f64().expect("series holds numbers")).collect())
            }
            other => panic!("{key}: unsupported value {other}"),
        };
        store.set(key, value);
    }
    store
}

fn ctx(pages: usize) -> RenderCtx {
    RenderCtx {
        now: Utc::now(),
        stale_secs: 300,
        page_index: 0,
        page_count: pages,
        message: "device message that is long enough to scroll on the ticker".into(),
        sources: vec![("metrics".into(), true), ("mempool".into(), false)],
    }
}

fn setup() -> (Config, Store) {
    let cfg = Config::parse(YAML, |k| std::env::var(k).ok()).expect("sample config parses");
    let store = store_from_json(JSON);
    store.set_at("sys/old", Value::Number(12.0), Utc::now() - Duration::seconds(3600));
    let (w, h) = (64u32, 48u32);
    let rgba: Vec<u8> = (0..w * h).flat_map(|i| [(i % w * 4) as u8, (i / w * 5) as u8, 180, 255]).collect();
    store.set("cam/front", Value::Image { width: w, height: h, rgba: Arc::new(rgba) });
    (cfg, store)
}

#[test]
fn sample_config_uses_every_widget_kind() {
    let (cfg, _) = setup();
    let kinds: HashSet<&str> = cfg
        .pages
        .iter()
        .flat_map(|p| &p.cards)
        .flat_map(|c| &c.widgets)
        .map(|w| match w {
            Widget::Kpi { .. } => "kpi",
            Widget::Gauge { .. } => "gauge",
            Widget::Hbar { .. } => "hbar",
            Widget::Spark { .. } => "spark",
            Widget::Bars { .. } => "bars",
            Widget::Text { .. } => "text",
            Widget::Rows { .. } => "rows",
            Widget::Dots { .. } => "dots",
            Widget::Ticker { .. } => "ticker",
            Widget::Image { .. } => "image",
        })
        .collect();
    assert_eq!(kinds.len(), 10, "{kinds:?}");
}

#[test]
fn every_sample_page_renders_a_non_blank_frame() {
    let (cfg, store) = setup();
    for (i, page) in cfg.pages.iter().enumerate() {
        let mut c = ctx(cfg.pages.len());
        c.page_index = i;
        for (w, h) in [(320, 170), (640, 340)] {
            let f = render_page(page, &store, &c, w, h);
            assert_eq!(f.rgba.len(), (w * h * 4) as usize);
            let colours: HashSet<&[u8; 4]> = f.rgba.as_chunks::<4>().0.iter().collect();
            assert!(colours.len() > 20, "page {} at {w}x{h} has only {} colours", page.name, colours.len());
            assert!(f.to_png().unwrap().len() > 1000);
        }
    }
}

#[test]
fn thirteen_card_page_timing() {
    let (cfg, store) = setup();
    let page = cfg.page("dense").expect("dense page");
    assert_eq!(page.cards.len(), 13);
    let c = ctx(cfg.pages.len());
    render_page(page, &store, &c, 320, 170);
    let runs = 20;
    let t = Instant::now();
    for _ in 0..runs {
        render_page(page, &store, &c, 320, 170);
    }
    let per = t.elapsed() / runs;
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    eprintln!("13-card page: {per:?} per frame ({profile})");
}
