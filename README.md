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
- **History charts** per card (temperature and fan, power vs limit, core clock, utilization and VRAM) for the last 15 minutes up to 30 days, or pick any past day to look at it afterwards. History is saved across reboots (30 days, one point per minute) and can be switched off with **Save history**
- **Reset to defaults** per card
- **Start with array** and start/stop/restart from the same page
- Works on phones and with Unraid themes
- Tiny footprint: one native binary of about 800 KB using roughly 3 MB of RAM, nothing else to install

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
- Live readings and the last 6 hours of history (every 5 seconds) are kept in RAM.
- Saved history is gentle on the flash drive: per-minute averages are written in one batch every 10 minutes and when the service stops (about 150 small writes a day, roughly 5 MB for 30 days), and files older than 30 days are deleted. It lives in `/boot/config/plugins/gputuner/data/history`, one file per day, next to your saved settings. Turn **Save history** off to stop writing it.
- Stopping the service, or the array, hands fan control back to the NVIDIA driver. Power limits stay applied.

## AMD GPUs (experimental, in testing)

AMD cards that use the Linux `amdgpu` driver are detected automatically and show up next to NVIDIA cards. **This support is in testing and has not been verified on real hardware. Changes are written to your real GPU, so use it at your own risk.**

- **What works:** live readings and history, a power cap (lower than the card's default only), and fan control with the same temperature safety floor as NVIDIA cards.
- **What does not work yet:** clock caps, throttle reasons, and fan curves on newer cards (RX 7000 and RX 9000 series) that only offer the newer fan-curve interface. On those, manual fan control may not work.
- **Try it safely first:** run `/usr/local/lib/gputuner/gputuner probe` in the Unraid terminal. It only reads. It prints what was detected and which amdgpu files exist, and writes nothing to the GPU. Open an issue with the output if something looks wrong.
- Fans are always handed back to the driver when the service stops.

## Notes

- GPUs passed through to a VM are not available to the plugin.
- Only run one GPU tuner at a time, so do not run it together with a container that does the same.
- Some cards are monitor-only (no fan or clock controls). The page locks those controls.
- A power loss can lose up to the last 10 minutes of saved history. Ranges longer than 6 hours show per-minute averages, so very short spikes are smoothed out.

## Uninstall

Go to **Plugins**, find GPUTuner, and click **Remove**. Your saved settings stay in `/boot/config/plugins/gputuner/data` so they survive a reinstall. Delete that folder for a clean slate.

## Requirements

- Unraid 6.12 or later (x86_64) with the NVIDIA Driver plugin
- About 6 MB of flash (the cached binary plus up to about 5 MB of saved history) and about 5 MB of RAM

## Development

```
rust/              the service: safety rules, fan loop, NVML access, sampler, socket server (cargo test, cargo build --release)
source/            files installed into the plugin (page, api, scripts)
assets/            logo.svg (source) and logo.png (rendered 256x256)
gputuner.plg       installer
update-hashes.sh   re-stamps the SHA256 of every downloaded file in the .plg
```

Push to `master` and the `release` workflow takes care of the rest: it runs the tests, builds the binary, publishes it as a GitHub release, sets the new version and the binary's checksum in `gputuner.plg` and `GPUTuner.xml`, adds your commit message to the changelog, and re-stamps the hashes. Write commit messages like changelog lines.

The service logic in `rust/src/safety.rs` and `rust/src/daemon.rs` is a port of the original gpu-tuner project (MIT), kept under its own licence in `rust/LICENSE-gpu-tuner`.

To run the page locally against fake data, render it in a browser with a stubbed `$.post`. To change the logo, edit `assets/logo.svg` and render it to `assets/logo.png`.

## License

MIT, see [LICENSE](LICENSE).
