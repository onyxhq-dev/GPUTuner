#!/usr/bin/env python3
# Plugin supervisor, adapted from 0sy/gpu-tuner-unraid container/entrypoint.py (MIT).
# Runs gpu-tunerd (root). The Unraid page talks to it through api.php/api.py, no separate web UI.
# SIGTERM stops the daemon and hands fan control back to the driver.
import json, os, signal, subprocess, sys

CFG = "/boot/config/plugins/gputuner/gputuner.cfg"
ROOT = "/usr/local/lib/gputuner"
RUN = "/run/gpu-tuner"
SOCK = RUN + "/control.sock"


def data_dir():
    d = "/mnt/user/appdata/gputuner"
    try:
        for line in open(CFG):
            k, _, v = line.strip().partition("=")
            if k == "DATA_DIR":
                d = v.strip().strip('"') or d
    except OSError:
        pass
    return d


def main():
    d = data_dir()
    for sub in ("config", "state"):
        os.makedirs(f"{d}/{sub}", mode=0o700, exist_ok=True)
    conf = d + "/config/config.json"
    if not os.path.exists(conf):  # allowed_uid null: root-only socket, which is all the page needs
        with open(conf, "w") as f:
            json.dump({"allowed_uid": None, "gpu_budget_w": None, "interval_s": 2.0}, f, indent=2)
    os.makedirs(RUN, exist_ok=True)
    os.chmod(RUN, 0o700)
    cmd = [sys.executable, ROOT + "/gpu-tunerd", "--config", conf, "--state", d + "/state/state.json", "--socket", SOCK]
    env = {**os.environ, "PYTHONPATH": ROOT + "/vendor", "PYTHONUNBUFFERED": "1"}
    daemon = subprocess.Popen(cmd, env=env)
    signal.signal(signal.SIGTERM, lambda *_: daemon.terminate())
    signal.signal(signal.SIGINT, lambda *_: daemon.terminate())
    try:
        rc = daemon.wait()
    finally:
        if daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
        subprocess.run(cmd + ["--restore-fans"], env=env, timeout=15)
    sys.exit(rc if rc > 0 else 0)


if __name__ == "__main__":
    main()
