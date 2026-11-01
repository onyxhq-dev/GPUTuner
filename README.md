<p align="center">
  <img src="assets/logo.png" alt="GPUTuner" width="180">
</p>

<h1 align="center">GPUTuner</h1>
<p align="center">NVIDIA GPU monitoring and tuning inside the Unraid web UI. A plugin, no Docker.</p>

<p align="center">
  <img src="https://img.shields.io/github/license/onyxhq-dev/GPUTuner?color=8892bf" alt="License">
  <img src="https://img.shields.io/badge/Unraid-6.12%2B-orange" alt="Unraid 6.12+">
  <img src="https://img.shields.io/github/last-commit/onyxhq-dev/GPUTuner" alt="Last commit">
</p>

<p align="center">
  Live readings, power limit, fan control, clock cap and history charts for every NVIDIA card,<br>
  on one page under Settings. No extra web server, port or password.
</p>

<br>

## Features

- **Live readings** every second: temperature, fan, power, core clock, utilization and VRAM, each with a usage bar
- **Throttle chips** that say what is limiting the card (idle, power cap, thermal limit, hardware slowdown)
- **Power limit** within the card's own range, with presets where a profile exists
- **Fan control**: driver default, a fixed speed or a curve preset, always above a temperature-based safety floor
- **Core clock cap** to run cooler and quieter
- **Combined power budget** across all cards
- **History charts** per card (temperature and fan, power vs limit, core clock, utilization and VRAM) for the last 15 minutes, 1 hour or 6 hours
- **Reset to defaults** per card
- **Start with array** and start/stop/restart from the same page
- Works on phones and with Unraid themes
- Python is bundled, nothing else to install

## Installation

Requires the **NVIDIA Driver** plugin (the GPU must show up in `nvidia-smi`).

1. In Unraid, go to **Plugins** and click **Install Plugin**
2. Paste this URL and click **Install**:

   ```
   https://raw.githubusercontent.com/onyxhq-dev/GPUTuner/master/gputuner.plg
   ```

## Usage

1. Go to **Settings > GPU Tuner**
2. Click **Start**, and set **Start with array** to `Yes` if you want it to come up after a reboot
3. Each GPU gets a card with live readings, three control panels (Power limit, Fan, Core clock cap) and history charts
4. Change a value and click that panel's **Apply**

Updating the plugin restarts the service by itself if it was running.

## How it works

- A small root service applies your settings to the GPU and re-checks every change: power range, power budget and the fan safety floor. The page cannot bypass it.
- The page talks to that service through Unraid's own authenticated web UI. Nothing listens on the network.
- Live readings and history are sampled in RAM. Nothing is written to the flash drive except your saved settings (`/boot/config/plugins/gputuner/data`, two small JSON files).
- Stopping the service, or the array, hands fan control back to the NVIDIA driver. Power limits stay applied.

## Notes

- GPUs passed through to a VM are not available to the plugin.
- Only run one GPU tuner at a time, so do not run it together with a container that does the same.
- Some cards are monitor-only (no fan or clock controls). The page locks those controls.
- History is kept in RAM, so it starts empty after a reboot.

## Uninstall

Go to **Plugins**, find GPUTuner, and click **Remove**. Your saved settings stay in `/boot/config/plugins/gputuner/data` so they survive a reinstall. Delete that folder for a clean slate.

## Requirements

- Unraid 6.12 or later (x86_64) with the NVIDIA Driver plugin
- About 35 MB of flash for the cached download and about 90 MB of RAM for the unpacked copy (recreated on every boot)

## Development

```
source/            files installed into the plugin (page, api, supervisor, scripts)
assets/            logo.svg (source) and logo.png (rendered 256x256)
gputuner.plg       installer
update-hashes.sh   re-stamps the SHA256 of every downloaded file in the .plg
```

Push to `master` and the `version-bump` workflow takes care of the rest: it sets the new version in `gputuner.plg` and `GPUTuner.xml`, adds your commit message to the changelog, and re-stamps the hashes. Write commit messages like changelog lines.

To run the page locally against fake data, render it in a browser with a stubbed `$.post`. To change the logo, edit `assets/logo.svg` and render it to `assets/logo.png`.

## License

MIT, see [LICENSE](LICENSE).
