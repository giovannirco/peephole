# Peephole firmware

ESPHome configurations, one per board. The board is a thin client: it fetches a PNG frame from the hub every few seconds and shows it; two buttons go next/previous; a long press toggles the backlight.

| Board | File | Notes |
|---|---|---|
| LilyGo T-Display-S3 | `peephole-tdisplay-s3.yaml` | ESP32-S3, 320×170 ST7789, 8 MB PSRAM, native USB (Improv over `USB_SERIAL_JTAG`) |

## Flash from the browser
Open the hub's `/flash/` page in Chrome or Edge over HTTPS, plug the board in, click **Install**. ESP Web Tools erases, writes the factory image, then asks for Wi-Fi (Improv). Afterwards open the board's own page (`http://<board ip>/`) and set **Hub URL** to your hub (`https://peephole.example.com`). The board registers itself with the hub on its first frame; name it in the hub UI.

## Build it yourself
```bash
uvx esphome compile peephole-tdisplay-s3.yaml
uvx esphome upload  peephole-tdisplay-s3.yaml --device /dev/cu.usbmodem*
```
Private build with Wi-Fi and the hub URL baked in: see `substitutions.example.yaml`.

## Adding a board
Copy the T-Display file, change `esp32`, `spi`, `display` (any ESPHome display works: the frame is an image), buttons and the `width`/`height` substitutions. The hub renders at whatever size the board asks for.
