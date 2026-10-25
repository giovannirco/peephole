# Design

## Shape
Hub and thin device. The hub (Rust: axum, tokio, tiny-skia, fontdue, reqwest) runs the plugins, holds every credential, renders pages as PNG frames at each device's resolution and serves a web UI and a flash page. A board fetches its frame every few seconds and reports button presses. The board holds no credential and no logic beyond "show this image".

Why not render on the board? The first version did (ESPHome lambdas drawing 13 pages in C++). It worked, but every data source needed a firewall hole from the IoT network to the source, credentials sat on the board, long PromQL URLs hit the HTTP client's buffer limits, and nobody but its author would ever write a page. The hub model fixes all four and makes the browser view free.

## Values
`plugin/key[/label]` → `Number | Text | Series | Image`, each with `updated_at`. Plugins write; the renderer reads. Stale values (older than `hub.stale_s`) draw grey with their age; missing values draw `--`.

## Pages
A page is a list of cards in a 320×170 design space (configurable per page); a card is a rounded rectangle with an accent bar and a list of widgets bound to keys. The renderer draws at the design size and resamples to the size the device asked for. Per-size layouts are a later feature.

## Devices
Registered on their first `GET /frame.png?id=…&w=…&h=…`. Each has a current page, a cycle period, a message line and an optional *hold* (a plugin event showing one page for N seconds). Buttons reset the cycle timer. State persists to a JSON file when `PEEPHOLE_STATE` is set.

## HTTP
| Route | Purpose |
|---|---|
| `GET /frame.png?id&w&h` | the device's current frame (registers the device) |
| `GET /pages/{name}.png?w&h` | any page, for the browser |
| `GET /api/pages`, `/api/devices`, `/api/devices/{id}`, `/api/values`, `/api/config` | read |
| `POST /api/devices/{id}/next|prev|page|message|name|cycle` | the only actions (403 in demo mode) |
| `GET /flash/`, `/flash/manifest.json`, `/firmware/*` | ESP Web Tools flashing |
| `GET /healthz` | liveness |

## Security posture
Read-only by design: plugins read, the device never holds a credential, the hub's actions change nothing outside the hub. Put the hub behind your ingress with TLS (WebSerial requires HTTPS anyway). The public demo runs with `PEEPHOLE_DEMO=1`: synthetic data, actions refused.

## Out of scope for now
Home Assistant integration, OTA from the hub, WASM plugins (any language), a static WASM build of the renderer for GitHub Pages, per-size layouts, write actions against Proxmox or Kubernetes.
