//! Render every page of a config to PNG files.
//!
//! `cargo run -p peephole-render --example snapshot -- <config.yaml> <values.json> <outdir>`
//!
//! `values.json` is an object of `key: number | "text" | [numbers]`; every value is stamped now.

use std::path::PathBuf;

use anyhow::{Context, bail};
use chrono::Utc;
use peephole_core::{Config, Store, Value};
use peephole_render::{RenderCtx, render_page};

fn store_from_json(text: &str) -> anyhow::Result<Store> {
    let store = Store::new();
    let serde_json::Value::Object(map) = serde_json::from_str(text)? else {
        bail!("values JSON must be an object of key: number | text | [numbers]");
    };
    for (key, v) in map {
        let value = match v {
            serde_json::Value::Number(n) => Value::Number(n.as_f64().context(format!("{key}: not a finite number"))?),
            serde_json::Value::String(s) => Value::Text(s),
            serde_json::Value::Array(a) => Value::Series(
                a.iter()
                    .map(|x| x.as_f64().context(format!("{key}: series must hold numbers")))
                    .collect::<Result<_, _>>()?,
            ),
            other => bail!("{key}: unsupported value {other}"),
        };
        store.set(key, value);
    }
    Ok(store)
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [config, values, outdir] = args.as_slice() else {
        bail!("usage: snapshot <config.yaml> <values.json> <outdir>");
    };
    let text = std::fs::read_to_string(config).with_context(|| format!("reading {config}"))?;
    let cfg = Config::parse(&text, |k| std::env::var(k).ok())?;
    let store = store_from_json(&std::fs::read_to_string(values).with_context(|| format!("reading {values}"))?)?;
    let outdir = PathBuf::from(outdir);
    std::fs::create_dir_all(&outdir)?;
    for (i, page) in cfg.pages.iter().enumerate() {
        let ctx = RenderCtx {
            now: Utc::now(),
            stale_secs: cfg.hub.stale_s,
            page_index: i,
            page_count: cfg.pages.len(),
            message: cfg.hub.title.clone(),
            sources: vec![],
        };
        let frame = render_page(page, &store, &ctx, page.width, page.height);
        let path = outdir.join(format!("{:02}-{}.png", i + 1, page.name));
        std::fs::write(&path, frame.to_png()?).with_context(|| format!("writing {}", path.display()))?;
        println!("{}", path.display());
    }
    Ok(())
}
