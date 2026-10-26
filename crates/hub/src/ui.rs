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
  <nav><a href="/settings">Settings</a><a href="/flash/">Flash a board</a><a href="/api/values">Values</a></nav>
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

const SETTINGS_STYLE: &str = r#"
.dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 6px; background: var(--text2); }
.dot.ok { background: var(--ok); } .dot.bad { background: var(--bad); }
.cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 16px; }
.card { background: var(--card); border: 1px solid var(--edge); border-radius: 10px; padding: 12px 14px; }
.card h3 { margin: 0 0 6px; font-size: 15px; display: flex; align-items: center; gap: 8px; }
.kind { font-size: 11px; padding: 1px 8px; border-radius: 999px; background: #1b2a3d; color: var(--accent); }
.meta { color: var(--text2); font-size: 12px; margin: 4px 0; }
.error { color: var(--bad); font-size: 12px; overflow-wrap: anywhere; }
details { margin-top: 8px; } summary { cursor: pointer; color: var(--text2); }
details ul { margin: 4px 0; padding-left: 18px; } .env { color: var(--accent); } .secret { color: var(--text2); }
.field { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin: 8px 0; }
.field > span:first-child { width: 90px; color: var(--text2); }
.checks label { margin-right: 12px; white-space: nowrap; }
input.num { width: 80px; } input[type=checkbox] { width: auto; }
#status.ok { color: var(--ok); }
"#;

const SETTINGS: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Settings · %%TITLE%%</title>
<style>%%STYLE%%%%SETTINGS_STYLE%%</style>
</head>
<body>
<header>
  <h1>Settings</h1>%%BADGE%%
  <nav><a href="/">Back to %%TITLE%%</a><a href="/flash/">Flash a board</a></nav>
</header>
<main>
  <p class="note">The hub is configured by its YAML file and its environment: change them and restart the hub. Only device settings change here.</p>
  %%TOKEN%%
  <div id="status"></div>
  <h2>Hub</h2>
  <table><thead><tr><th>Setting</th><th>Value</th><th>Set by</th></tr></thead><tbody id="hub"></tbody></table>
  <h2>Environment</h2>
  <table><thead><tr><th>Variable</th><th></th></tr></thead><tbody id="env"></tbody></table>
  <h2>Plugins</h2>
  <div class="cards" id="plugins"></div>
  <h2>Devices</h2>
  <div class="cards" id="devices"></div>
</main>
<script>
const statusLine = document.getElementById('status');
const tokenInput = document.getElementById('token');

function el(tag, text, cls) {
  const e = document.createElement(tag);
  if (text !== undefined) e.textContent = text;
  if (cls) e.className = cls;
  return e;
}

function say(text, ok) { statusLine.textContent = text; statusLine.className = ok ? 'ok' : ''; }

async function send(path, body) {
  const opts = { method: 'POST', headers: {} };
  if (body !== undefined) { opts.headers['content-type'] = 'application/json'; opts.body = JSON.stringify(body); }
  if (tokenInput && tokenInput.value) opts.headers['authorization'] = 'Bearer ' + tokenInput.value;
  const res = await fetch(path, opts);
  if (!res.ok) {
    const err = await res.json().catch(() => ({ error: res.statusText }));
    say(err.error || res.statusText, false);
    return false;
  }
  say('Saved.', true);
  return true;
}

function ago(s) {
  if (s === null || s === undefined) return 'never';
  if (s < 60) return s + 's ago';
  if (s < 3600) return Math.floor(s / 60) + 'm ago';
  return Math.floor(s / 3600) + 'h ago';
}

function dot(ok) { return el('span', undefined, 'dot' + (ok === true ? ' ok' : ok === false ? ' bad' : '')); }

function row(cells) { const tr = el('tr'); cells.forEach(c => { const td = el('td'); td.append(c); tr.append(td); }); return tr; }

function envText(v) {
  let t = 'env ' + v.env + ' · ' + (v.set ? 'set' : 'unset');
  if (v.default !== undefined) t += ' · default ' + v.default;
  return el('span', t, 'env');
}

function shown(v) {
  if (v === null) return document.createTextNode('null');
  if (Array.isArray(v)) { const ul = el('ul'); v.forEach(x => { const li = el('li'); li.append(shown(x)); ul.append(li); }); return ul; }
  if (typeof v === 'object') {
    if (v.env !== undefined) return envText(v);
    if (v.envs !== undefined) { const span = el('span'); v.envs.forEach((e, i) => { if (i) span.append(', '); span.append(envText(e)); }); return span; }
    if (v.secret) return el('span', '••••', 'secret');
    const ul = el('ul');
    Object.keys(v).forEach(k => { const li = el('li', k + ': '); li.append(shown(v[k])); ul.append(li); });
    return ul;
  }
  return document.createTextNode(String(v));
}

function renderHub(h) {
  const yes = b => b ? 'yes' : 'no';
  const rows = [
    ['Title', h.title, 'config file (hub.title)'], ['Cycle', h.cycle_s + ' s', 'config file (hub.cycle_s)'],
    ['Stale after', h.stale_s + ' s', 'config file (hub.stale_s)'], ['Demo mode', yes(h.demo), 'PEEPHOLE_DEMO'],
    ['Changes need a token', yes(h.auth), 'PEEPHOLE_API_TOKEN'], ['Listen', h.listen, 'PEEPHOLE_LISTEN'],
    ['Public URL', h.public_url || '—', 'PEEPHOLE_PUBLIC_URL'], ['Device state file', yes(h.state_file), 'PEEPHOLE_STATE'],
    ['Firmware dir', yes(h.firmware_dir), 'PEEPHOLE_FIRMWARE_DIR'], ['Config file', h.config_path || 'built-in demo', 'PEEPHOLE_CONFIG'],
  ];
  document.getElementById('hub').replaceChildren(...rows.map(r => row([r[0], r[1], el('code', r[2])])));
  document.getElementById('env').replaceChildren(...h.env.map(e => {
    const state = el('span'); state.append(dot(e.set ? true : undefined), e.set ? 'set' : 'unset');
    return row([el('code', e.name), state]);
  }));
}

function renderPlugin(p) {
  const card = el('div', undefined, 'card');
  const h3 = el('h3'); h3.append(dot(p.ok), p.name, el('span', p.kind, 'kind'));
  const interval = p.interval_s === null ? '' : 'every ' + p.interval_s + ' s · ';
  const values = p.values === null ? '' : ' · ' + p.values + ' values';
  card.append(h3, el('div', interval + 'last poll ' + ago(p.age_s) + values, 'meta'));
  if (p.error) card.append(el('div', p.error, 'error'));
  const poll = el('button', 'Poll now');
  poll.onclick = async () => { if (await send('/api/plugins/' + encodeURIComponent(p.name) + '/poll')) setTimeout(load, 1000); };
  card.append(poll);
  const details = el('details'); details.append(el('summary', 'Settings'), shown(p.settings));
  card.append(details);
  return card;
}

function field(label, ...controls) { const f = el('div', undefined, 'field'); f.append(el('span', label), ...controls); return f; }

function renderDevice(d, pages) {
  const base = '/api/devices/' + encodeURIComponent(d.id) + '/';
  const after = ok => { if (ok) load(); };
  const card = el('div', undefined, 'card');
  card.append(el('h3', d.name || d.id), el('div', d.id + ' · ' + d.width + '×' + d.height, 'meta'));
  const name = el('input'); name.value = d.name || ''; name.maxLength = 64; name.placeholder = 'Name';
  const saveName = el('button', 'Save'); saveName.onclick = async () => after(await send(base + 'name', { name: name.value }));
  const cycle = el('input', undefined, 'num'); cycle.type = 'number'; cycle.min = 0; cycle.max = 86400; cycle.value = d.cycle_s;
  const saveCycle = el('button', 'Save'); saveCycle.onclick = async () => after(await send(base + 'cycle', { cycle_s: Number(cycle.value) }));
  const page = el('select');
  pages.forEach((n, i) => {
    if (d.pages.length && !d.pages.includes(n)) return;
    const o = el('option', n); o.value = n; o.selected = i === d.page; page.append(o);
  });
  const savePage = el('button', 'Show'); savePage.onclick = async () => after(await send(base + 'page', { name: page.value }));
  const checks = el('div', undefined, 'checks');
  pages.forEach(n => {
    const label = el('label'); const box = el('input'); box.type = 'checkbox'; box.value = n; box.checked = d.pages.includes(n);
    label.append(box, ' ' + n); checks.append(label);
  });
  const savePages = el('button', 'Save');
  savePages.onclick = async () => {
    const chosen = [...checks.querySelectorAll('input:checked')].map(b => b.value);
    after(await send(base + 'pages', { pages: chosen }));
  };
  card.append(field('Name', name, saveName), field('Cycle (s)', cycle, saveCycle), field('Page', page, savePage),
    field('Page list', checks, savePages), el('div', 'No page ticked = every page.', 'meta'));
  return card;
}

async function load() {
  const data = await (await fetch('/api/settings')).json();
  renderHub(data.hub);
  document.getElementById('plugins').replaceChildren(...data.plugins.map(renderPlugin));
  const devices = data.devices.map(d => renderDevice(d, data.pages));
  document.getElementById('devices').replaceChildren(...(devices.length ? devices : [el('p', 'No board has fetched a frame yet.', 'empty')]));
}
load().catch(e => say(String(e), false));
</script>
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

/// The settings panel: shows the hub, its variables and plugins (redacted) and edits device settings.
/// The token input (only when `PEEPHOLE_API_TOKEN` is set) keeps the token in the page's memory.
pub fn settings(hub: &Hub) -> String {
    let badge = if hub.demo { r#" <span class="badge">DEMO</span>"# } else { "" };
    let token = if hub.api_token.is_some() {
        r#"<p class="field"><span>API token</span><input id="token" type="password" autocomplete="off" placeholder="Needed to save changes"></p>"#
    } else {
        ""
    };
    SETTINGS
        .replace("%%STYLE%%", STYLE)
        .replace("%%SETTINGS_STYLE%%", SETTINGS_STYLE)
        .replace("%%TITLE%%", &escape(&hub.cfg.hub.title))
        .replace("%%BADGE%%", badge)
        .replace("%%TOKEN%%", token)
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
