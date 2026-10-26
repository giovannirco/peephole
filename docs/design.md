# Design

## Shape
Hub and thin device. The hub (Rust: axum, tokio, tiny-skia, fontdue, reqwest) runs the plugins, holds every credential, renders pages as PNG frames at each device's resolution and serves a web UI and a flash page. A board fetches its frame every few seconds and reports button presses. The board holds no credential and no logic beyond "show this image".

Why not render on the board? The first version did (ESPHome lambdas drawing 13 pages in C++). It worked, but every data source needed a firewall hole from the IoT network to the source, credentials sat on the board, long PromQL URLs hit the HTTP client's buffer limits, and nobody but its author would ever write a page. The hub model fixes all four and makes the browser view free.

## Values
`plugin/key[/label]` → `Number | Text | Series | Image`, each with `updated_at`. Plugins write; the renderer reads. Stale values (older than `hub.stale_s`) draw grey with their age; missing values draw `--`.

## Pages
A page is a list of cards in a 320×170 design space (configurable per page); a card is a rounded rectangle with an accent bar and a list of widgets bound to keys. The renderer lays every page out in that design space and delivers the size the device asked for. Per-size layouts are a later feature.

Pages are drawn at a uniform scale of at least 1, integer or fractional (the smaller of requested width / design width and requested height / design height): every primitive takes design-space coordinates, paths and strokes go through the scale, glyphs are rasterized at the scaled font size, and text is still measured in design space, so the layout is the same at every size. A 2× or 3× frame (the browser view, the GitHub profile cards) is drawn sharp rather than upscaled; the boards ask for their native size and still get the 1× frame, pixel for pixel. Only a target smaller than the design, or one of another aspect, is resampled.

## Devices
Registered on their first `GET /frame.png?id=…&w=…&h=…`. Each has a current page, a cycle period, a message line and an optional *hold* (a plugin event showing one page for N seconds). Buttons reset the cycle timer. State persists to a JSON file when `PEEPHOLE_STATE` is set.

## HTTP
| Route | Purpose |
|---|---|
| `GET /frame.png?id&w&h` | the device's current frame (registers the device) |
| `GET /pages/{name}.png?w&h` | any page, for the browser |
| `GET /api/pages`, `/api/devices`, `/api/devices/{id}`, `/api/values`, `/api/config`, `/api/plugins`, `/api/settings` | read |
| `POST /api/devices/{id}/next|prev|page|message|name|cycle|pages`, `POST /api/plugins/{name}/poll` | the only actions (403 in demo mode, 401 without the token when `PEEPHOLE_API_TOKEN` is set) |
| `GET /settings` | the settings panel |
| `GET /flash/`, `/flash/manifest.json`, `/firmware/*` | ESP Web Tools flashing |
| `GET /healthz` | liveness |

## Security posture
Read-only by design: plugins read, the device never holds a credential, the hub's actions change nothing outside the hub. Put the hub behind your ingress with TLS (WebSerial requires HTTPS anyway). The public demo runs with `PEEPHOLE_DEMO=1`: synthetic data, actions refused.

## v0.2
- **Settings panel shows, does not edit.** The YAML and the environment stay the configuration. `/settings` and `/api/settings` show them from the *unexpanded* text: a `${VAR}` appears by name with whether it is set (its default too, except under a secret key); strings, numbers and booleans under keys that look secret, and every header, appear as `{"secret": true}`; URLs lose their userinfo and secret query values; poll errors go through the same URL filter. No expanded value reaches a response. Only device settings are editable.
- **Page lists.** A device may carry `pages`, an ordered subset of page names. Buttons and the cycle move inside it (wrapping); from a page outside it they land on its first entry; `page` by name must be in it; a plugin event (hold) may still show any page. Empty = every page.
- **Poll now.** The scheduler wakes a plugin's loop on demand without moving its interval; each poll writes `<plugin>/_polled_at`, and `<plugin>/_values` on success.
- **Optional token.** `PEEPHOLE_API_TOKEN` guards every `POST` with a bearer token compared without early exit; GETs stay open for boards and dashboards. Demo mode refuses POSTs first (403).

## Out of scope for now
Home Assistant integration, OTA from the hub, WASM plugins (any language), a static WASM build of the renderer for GitHub Pages, per-size layouts, write actions against Proxmox or Kubernetes.
