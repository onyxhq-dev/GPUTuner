<p align="center">
  <img src="assets/logo.png" alt="GPUTuner" width="180">
</p>

<h1 align="center">GPUTuner</h1>
<p align="center">NVIDIA GPU power, fan and clock control inside the Unraid web UI - as a plugin, no Docker.</p>

<p align="center">
  <img src="https://img.shields.io/github/license/onyxhq-dev/GPUTuner?color=8892bf" alt="License">
  <img src="https://img.shields.io/badge/Unraid-6.12%2B-orange" alt="Unraid 6.12+">
  <img src="https://img.shields.io/github/last-commit/onyxhq-dev/GPUTuner" alt="Last commit">
</p>

GPUTuner is a native Unraid plugin built on the `gpu-tunerd` daemon from [0sy/gpu-tuner-unraid](https://github.com/0sy/gpu-tuner-unraid) (MIT),
which packages [willboyden/gpu-tuner](https://github.com/willboyden/gpu-tuner) as a container. Unlike the container there is **no separate web UI,
port or password**: everything lives on one page inside the Unraid web UI (Settings > Utilities > GPU Tuner).
The daemon code is downloaded at install time (pinned and checksummed).

## Features

- **Live readings** per card: temperature, fan, power, clocks, utilization, VRAM, throttle reasons
- **Power limit** within the card's own NVML range, with presets where a profile exists, plus an optional combined GPU power budget
- **Fan control** - driver default, fixed speed or a curve preset, always above a temperature-based safety floor
- **Core clock cap** to run cooler and quieter
- **Reset to defaults** per card
- **Start with array**, start/stop/restart from the same page
- Every change is validated by the root daemon, not the page. Stopping the service (or the array) hands fan control back to the NVIDIA driver; power caps stay applied

## Installation

Requires the **NVIDIA Driver** plugin (GPU visible in `nvidia-smi`). Python 3.12 is bundled with the plugin, nothing else to install.

1. In Unraid, go to **Plugins** and click **Install Plugin**
2. Paste this URL and click **Install**:

   ```
   https://raw.githubusercontent.com/onyxhq-dev/GPUTuner/master/gputuner.plg
   ```

## Usage

1. Go to **Settings > Utilities > GPU Tuner**
2. Set **Start with array** to `Yes`, click **Apply**, then **Start**
3. The GPU panels appear below; change a value and click its **Apply** button

State (saved settings, budget) lives in `/mnt/user/appdata/gputuner` by default.

## Notes

- The daemon runs as root (NVML writes need it). The page reaches it only through Unraid's own authenticated webGUI (CSRF-checked) over a root-only socket. Nothing listens on the network.
- GPUs passed through to a VM are not available. Only one tuner should manage the same cards.
- Some GPU models are monitor-only (no fan or clock controls); the page locks those controls.
- Do not run this together with the Docker container version.

## Development

After editing anything in `source/` or `assets/`, run `./update-hashes.sh` to re-stamp the SHA256s in `gputuner.plg`,
and bump `version` in `gputuner.plg` and `GPUTuner.xml`. To update upstream, change `upver` and its SHA256 in the `.plg`.

## Uninstall

Go to **Plugins**, find GPUTuner, and click **Remove**. The data directory is kept.

## Requirements

- Unraid 6.12 or later (x86_64), NVIDIA Driver plugin
- About 35 MB of flash for the cached Python download, and about 90 MB of RAM for the unpacked copy (re-created on every boot)

## License

MIT - see [LICENSE](LICENSE). Upstream license: [UPSTREAM-LICENSE](UPSTREAM-LICENSE).
