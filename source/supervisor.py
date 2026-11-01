#!/usr/bin/env python3
# Plugin supervisor, adapted from 0sy/gpu-tuner-unraid container/entrypoint.py (MIT).
import json, os, signal, subprocess, sys, threading, time

DATA = "/boot/config/plugins/gputuner/data"
ROOT = "/usr/local/lib/gputuner"
RUN = "/run/gpu-tuner"
SOCK = RUN + "/control.sock"
HISTORY = RUN + "/history.jsonl"
LIVE = RUN + "/live.json"
HISTORY_EVERY, KEEP = 5, 4320


def sampler():
    sys.path[:0] = [ROOT, ROOT + "/vendor"]
    try:
        from gpu_tuner.nvml import Nvml
        nv = Nvml()
    except Exception:
        return
    tick, nxt = 0, time.monotonic()
    while True:
        samples, row = {}, {}
        for g in nv.gpus:
            try:
                s = nv.sample(g)
            except Exception:
                continue
            samples[g.uuid] = s
            fans = [f for f in s["fans"] if f is not None]
            row[g.uuid] = [s["temp"], max(fans) if fans else None, s["power_w"], s["power_limit_w"],
                           s["clock_mhz"], s["util"], s["vram_used_mib"], s["vram_total_mib"]]
        now = int(time.time())
        with open(LIVE + ".tmp", "w") as f:
            json.dump({"t": now, "gpus": samples}, f, default=str)
        os.replace(LIVE + ".tmp", LIVE)
        tick += 1
        if tick % HISTORY_EVERY == 0:
            with open(HISTORY, "a") as f:
                f.write(json.dumps({"t": now, "g": row}) + "\n")
            if tick % (HISTORY_EVERY * 120) == 0:
                with open(HISTORY) as f:
                    lines = f.readlines()[-KEEP:]
                with open(HISTORY + ".tmp", "w") as f:
                    f.writelines(lines)
                os.replace(HISTORY + ".tmp", HISTORY)
        nxt += 1
        time.sleep(max(0, nxt - time.monotonic()))


def main():
    d = DATA
    for sub in ("config", "state"):
        os.makedirs(f"{d}/{sub}", mode=0o700, exist_ok=True)
    conf = d + "/config/config.json"
    if not os.path.exists(conf):
        with open(conf, "w") as f:
            json.dump({"allowed_uid": None, "gpu_budget_w": None, "interval_s": 2.0}, f, indent=2)
    os.makedirs(RUN, exist_ok=True)
    os.chmod(RUN, 0o700)
    cmd = [sys.executable, ROOT + "/gpu-tunerd", "--config", conf, "--state", d + "/state/state.json", "--socket", SOCK]
    env = {**os.environ, "PYTHONPATH": ROOT + "/vendor", "PYTHONUNBUFFERED": "1"}
    daemon = subprocess.Popen(cmd, env=env)
    signal.signal(signal.SIGTERM, lambda *_: daemon.terminate())
    signal.signal(signal.SIGINT, lambda *_: daemon.terminate())
    threading.Thread(target=sampler, daemon=True).start()
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
