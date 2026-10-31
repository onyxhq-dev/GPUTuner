<p align="center">
  <img src="assets/logo.png" alt="GPUTuner" width="180">
</p>

<h1 align="center">GPUTuner</h1>
<p align="center">NVIDIA GPU power, fan and clock control for Unraid - as a plugin, no Docker.</p>

<p align="center">
  <img src="https://img.shields.io/github/license/onyxhq-dev/GPUTuner?color=8892bf" alt="License">
  <img src="https://img.shields.io/badge/Unraid-6.12%2B-orange" alt="Unraid 6.12+">
  <img src="https://img.shields.io/github/last-commit/onyxhq-dev/GPUTuner" alt="Last commit">
</p>

GPUTuner is the plugin version of [0sy/gpu-tuner-unraid](https://github.com/0sy/gpu-tuner-unraid) (MIT), which
packages [willboyden/gpu-tuner](https://github.com/willboyden/gpu-tuner) as a container. The plugin downloads that
code (pinned and checksummed) at install time and adds an Unraid settings page and service management.

## Features

- **Power limit** per card, within the card's own NVML range, with an optional combined GPU power budget
- **Fan control** - curve, fixed speed or driver default, always above a temperature-based safety floor
- **Core clock cap** to run cooler and quieter
- **Live monitoring** - temps, fan, power, clocks, utilization, VRAM, 6 hours of charts
- **Start with array**, start/stop/restart from the Unraid settings page
- Stopping the service (or the array) hands fan control back to the NVIDIA driver; power caps stay applied

## Installation

Requires the **NVIDIA Driver** plugin (GPU visible in `nvidia-smi`) and `python3` on the host.

1. In Unraid, go to **Plugins** and click **Install Plugin**
2. Paste this URL and click **Install**:

   ```
   https://raw.githubusercontent.com/onyxhq-dev/GPUTuner/master/gputuner.plg
   ```

## Usage

1. Go to **Settings > Utilities > GPUTuner**
2. Set a **WebUI password** (12+ characters) - the service will not start without one
3. Set **Start with array** to `Yes` and click **Apply**, then **Start**
4. Click **Open GPU Tuner** (default port 8765)

Data (state, password hash) lives in `/mnt/user/appdata/gputuner` by default.

## Notes

- The daemon runs as root (NVML writes need it); the web UI runs as `nobody`. The UI is plain HTTP, for a trusted LAN only.
- GPUs passed through to a VM are not available. Only one tuner should manage the same cards.
- Do not reuse the Docker container's appdata folder (it uses uid 10001; the plugin uses `nobody`).
- Forgot the password: set a new one on the settings page.

## Development

After editing anything in `source/` or `assets/`, run `./update-hashes.sh` to re-stamp the SHA256s in `gputuner.plg`,
and bump `version` in `gputuner.plg` and `GPUTuner.xml`. To update upstream, change `upver` and its SHA256 in the `.plg`.

## Uninstall

Go to **Plugins**, find GPUTuner, and click **Remove**. The data directory is kept.

## Requirements

- Unraid 6.12 or later, NVIDIA Driver plugin, python3

## License

MIT - see [LICENSE](LICENSE). Upstream license: [UPSTREAM-LICENSE](UPSTREAM-LICENSE).
