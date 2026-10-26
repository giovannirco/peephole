use crate::firmware::Firmware;
use crate::state::Hub;

const STYLE: &str = r#"
:root { color-scheme: dark; --bg: #0b0e14; --card: #131822; --edge: #222a38; --text: #e6e9ef; --text2: #9aa4b5;
  --accent: #6cb6ff; --bad: #ff6b6b; --ok: #4cd18a; }
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--text); font: 14px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; }
header { display: flex; align-items: center; gap: 12px; padding: 14px 20px; border-bottom: 1px solid var(--edge); }
header h1 { font-size: 18px; margin: 0; font-weight: 600; }
header nav { margin-left: auto; display: flex; gap: 16px; }
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
.badge { font-size: 11px; font-weight: 700; letter-spacing: .08em; padding: 2px 8px; border-radius: 999px; background: #3b2a12; color: #ffb547; }
main { padding: 20px; max-width: 1100px; margin: 0 auto; }
h2 { font-size: 13px; text-transform: uppercase; letter-spacing: .08em; color: var(--text2); margin: 24px 0 12px; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 16px; }
figure { margin: 0; background: var(--card); border: 1px solid var(--edge); border-radius: 10px; padding: 10px; }
figure img { display: block; width: 320px; max-width: 100%; aspect-ratio: 320 / 170; border-radius: 6px; background: #000; }
figcaption { margin-top: 6px; color: var(--text2); font-size: 12px; }
table { width: 100%; border-collapse: collapse; background: var(--card); border: 1px solid var(--edge); border-radius: 10px; overflow: hidden; }
th, td { padding: 8px 10px; text-align: left; border-bottom: 1px solid var(--edge); vertical-align: middle; }
th { color: var(--text2); font-weight: 500; font-size: 12px; }
td.actions { display: flex; flex-wrap: wrap; gap: 6px; }
button, select, input { background: #1b2230; color: var(--text); border: 1px solid var(--edge); border-radius: 6px; padding: 4px 8px; font: inherit; }
button { cursor: pointer; }
button:hover { border-color: var(--accent); }
input { width: 180px; }
.empty, .note { color: var(--text2); }
#status { min-height: 1.4em; margin-top: 8px; color: var(--bad); }
ol li { margin: 6px 0; }
code { background: #1b2230; padding: 1px 6px; border-radius: 4px; }
.warn { background: #2a1416; border: 1px solid #5a2a2e; border-radius: 8px; padding: 10px 14px; }
"#;

const INDEX: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>%%TITLE%%</title>
<style>%%STYLE%%</style>
</head>
<body>
<header>
  <h1>%%TITLE%%</h1>%%BADGE%%
  <nav><a href="/flash/">Flash a board</a><a href="/api/values">Values</a></nav>
</header>
<main>
  <h2>Pages</h2>
  <div class="grid">%%PAGES%%</div>
  <h2>Devices</h2>
  <table>
    <thead><tr><th>ID</th><th>Name</th><th>Size</th><th>Page</th><th>Pages</th><th>Last seen</th><th>Frames</th><th></th></tr></thead>
    <tbody id="devices"><tr><td colspan="8" class="empty">Loading devices…</td></tr></tbody>
  </table>
  <div id="status"></div>
</main>
<script>
const statusLine = document.getElementById('status');
let pageNames = [];

function refreshImages() {
  const t = Date.now();
  document.querySelectorAll('img[data-src]').forEach(img => { img.src = img.dataset.src + '&t=' + t; });
}

function ago(iso) {
  const s = Math.max(0, Math.round((Date.now() - Date.parse(iso)) / 1000));
  if (s < 60) return s + 's ago';
  if (s < 3600) return Math.floor(s / 60) + 'm ago';
  return Math.floor(s / 3600) + 'h ago';
}

async function act(id, action, body) {
  const opts = { method: 'POST' };
  if (body !== undefined) { opts.headers = { 'content-type': 'application/json' }; opts.body = JSON.stringify(body); }
  const res = await fetch('/api/devices/' + encodeURIComponent(id) + '/' + action, opts);
  if (!res.ok) {
    const err = await res.json().catch(() => ({ error: res.statusText }));
    statusLine.textContent = err.error || res.statusText;
    return;
  }
  statusLine.textContent = '';
  loadDevices();
}

function el(tag, text) { const e = document.createElement(tag); if (text !== undefined) e.textContent = text; return e; }

function row(d) {
  const tr = document.createElement('tr');
  tr.append(el('td', d.id), el('td', d.name || '—'), el('td', d.width + '×' + d.height),
    el('td', pageNames[d.page] || String(d.page)), el('td', d.pages.length ? String(d.pages.length) : 'all'),
    el('td', ago(d.last_seen)), el('td', String(d.frames)));
  const td = el('td'); td.className = 'actions';
  const prev = el('button', 'Prev'); prev.onclick = () => act(d.id, 'prev');
  const next = el('button', 'Next'); next.onclick = () => act(d.id, 'next');
  const select = el('select');
  pageNames.forEach((n, i) => {
    if (d.pages.length && !d.pages.includes(n)) return;
    const o = el('option', n); o.value = n; o.selected = i === d.page; select.append(o);
  });
  select.onchange = () => act(d.id, 'page', { name: select.value });
  const input = el('input'); input.placeholder = 'Message'; input.maxLength = 200; input.value = d.message || '';
  input.onkeydown = e => { if (e.key === 'Enter') act(d.id, 'message', { text: input.value }); };
  const send = el('button', 'Send'); send.onclick = () => act(d.id, 'message', { text: input.value });
  td.append(prev, next, select, input, send);
  tr.append(td);
  return tr;
}

async function loadDevices() {
  if (document.activeElement && document.activeElement.closest('#devices')) return;
  const res = await fetch('/api/devices');
  const list = await res.json();
  const body = document.getElementById('devices');
  body.replaceChildren();
  if (list.length === 0) {
    const td = el('td', 'No board has fetched a frame yet.'); td.colSpan = 8; td.className = 'empty';
    const tr = el('tr'); tr.append(td); body.append(tr);
    return;
  }
  list.forEach(d => body.append(row(d)));
}

async function start() {
  pageNames = (await (await fetch('/api/pages')).json()).map(p => p.name);
  await loadDevices();
  setInterval(() => { refreshImages(); loadDevices(); }, 5000);
}
start().catch(e => { statusLine.textContent = String(e); });
</script>
</body>
</html>
"#;

const FLASH: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Flash a board · %%TITLE%%</title>
<style>%%STYLE%%</style>
<script type="module" src="https://unpkg.com/esp-web-tools@10/dist/web/install-button.js?module"></script>
</head>
<body>
<header>
  <h1>Flash a board</h1>
  <nav><a href="/">Back to %%TITLE%%</a></nav>
</header>
<main>
  %%FIRMWARE%%
  <p><esp-web-install-button manifest="/flash/manifest.json"></esp-web-install-button></p>
  <h2>Steps</h2>
  <ol>
    <li>Use Chrome or Edge on a desktop: flashing goes through WebSerial, which other browsers do not offer.</li>
    <li>Open this page over HTTPS (or on localhost): browsers only allow WebSerial on secure pages.</li>
    <li>Plug the board in by USB and press <b>Install</b>. If no serial port shows up, hold <b>BOOT</b> while plugging it in (or hold BOOT and press RST), then try again.</li>
    <li>After flashing, the installer offers to set up Wi-Fi (Improv): choose your network and enter its password.</li>
    <li>Open the board's own web page (its address is on the screen) and set the hub URL to %%HUB_URL%%.</li>
  </ol>
</main>
</body>
</html>
"#;

/// The dashboard: every page as a live image, and the devices with their controls.
pub fn index(hub: &Hub) -> String {
    let pages: String = hub
        .cfg
        .pages
        .iter()
        .map(|p| {
            let src = format!("/pages/{}.png?w=640&h=340", encode_segment(&p.name));
            let caption = p.title.as_deref().map_or_else(|| p.name.clone(), |t| format!("{t} · {}", p.name));
            format!(
                r#"<figure><img src="{src}" data-src="{src}" alt="{alt}" width="320" height="170"><figcaption>{caption}</figcaption></figure>"#,
                src = escape(&src),
                alt = escape(&p.name),
                caption = escape(&caption),
            )
        })
        .collect();
    let pages = if pages.is_empty() { r#"<p class="empty">No pages are configured.</p>"#.to_owned() } else { pages };
    let badge = if hub.demo { r#" <span class="badge">DEMO</span>"# } else { "" };
    INDEX
        .replace("%%STYLE%%", STYLE)
        .replace("%%TITLE%%", &escape(&hub.cfg.hub.title))
        .replace("%%BADGE%%", badge)
        .replace("%%PAGES%%", &pages)
}

/// The flash page: ESP Web Tools install button, steps and the hub URL to give the board.
pub fn flash(hub: &Hub, builds: &[Firmware]) -> String {
    let firmware = if builds.is_empty() {
        r#"<p class="warn">No firmware is available on this hub yet: set <code>PEEPHOLE_FIRMWARE_DIR</code> to a folder with <code>&lt;board&gt;-&lt;version&gt;.factory.bin</code> files.</p>"#.to_owned()
    } else {
        let list: Vec<String> = builds
            .iter()
            .map(|f| format!("{} {} ({})", escape(f.board), escape(&f.version), escape(f.chip_family)))
            .collect();
        format!(r#"<p class="note">Firmware on this hub: {}.</p>"#, list.join(", "))
    };
    let hub_url = match &hub.public_url {
        Some(url) => format!("<code>{}</code>", escape(url)),
        None => {
            "the address you use to reach this hub (set <code>PEEPHOLE_PUBLIC_URL</code> to show it here)".to_owned()
        }
    };
    FLASH
        .replace("%%STYLE%%", STYLE)
        .replace("%%TITLE%%", &escape(&hub.cfg.hub.title))
        .replace("%%FIRMWARE%%", &firmware)
        .replace("%%HUB_URL%%", &hub_url)
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn encode_segment(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_special_characters() {
        assert_eq!(escape(r#"<a href="x">&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;");
    }

    #[test]
    fn encodes_path_segments() {
        assert_eq!(encode_segment("alpha"), "alpha");
        assert_eq!(encode_segment("a b/é"), "a%20b%2F%C3%A9");
    }
}
