#!/usr/bin/env python3
# Plugin supervisor, adapted from 0sy/gpu-tuner-unraid container/entrypoint.py (MIT).
# Runs gpu-tunerd as root and the web UI as nobody. SIGTERM restores driver fan control.
#   supervisor.py                 run (foreground)
#   supervisor.py --set-password  read new WebUI password from stdin
import json, os, signal, subprocess, sys, tempfile, time

CFG = "/boot/config/plugins/gputuner/gputuner.cfg"
ROOT = "/usr/local/lib/gpu-tuner"
RUN = "/run/gpu-tuner"
SOCK = RUN + "/control.sock"
UID, GID = 99, 100  # nobody:users
sys.path.insert(0, ROOT)
sys.path.insert(0, ROOT + "/vendor")


def cfg():
    c = {"DATA_DIR": "/mnt/user/appdata/gpu-tuner", "PORT": "8765", "LAN_IP": ""}
    try:
        for line in open(CFG):
            k, _, v = line.strip().partition("=")
            if k in c:
                c[k] = v.strip().strip('"')
    except OSError:
        pass
    return c


def lan_ip(c):
    if c["LAN_IP"]:
        return c["LAN_IP"]
    out = subprocess.run(["ip", "-4", "route", "get", "1.1.1.1"], capture_output=True, text=True).stdout.split()
    return out[out.index("src") + 1] if "src" in out else ""


def daemon_cmd(c, *extra):
    d = c["DATA_DIR"]
    return [sys.executable, ROOT + "/gpu-tunerd", "--config", d + "/config/config.json",
            "--state", d + "/state/state.json", "--socket", SOCK, *extra]


def init_data(c):
    d = c["DATA_DIR"]
    os.makedirs(d, exist_ok=True)
    os.chmod(d, 0o711)
    for n in ("config", "state", "ui"):
        os.makedirs(f"{d}/{n}", exist_ok=True)
        os.chmod(f"{d}/{n}", 0o700)
    p = d + "/config/config.json"
    if not os.path.exists(p):
        with open(p, "w") as f:
            json.dump({"allowed_uid": UID, "gpu_budget_w": None, "interval_s": 2.0}, f, indent=2)
        os.chmod(p, 0o600)
    os.chown(d + "/ui", UID, GID)


def set_password(c):
    from gpu_tuner.password_auth import hash_password
    init_data(c)
    stored = hash_password(sys.stdin.read().rstrip("\n"))  # raises on bad length
    cdir = c["DATA_DIR"] + "/ui/.config/gpu-tuner"
    for p in (os.path.dirname(cdir), cdir):
        os.makedirs(p, mode=0o700, exist_ok=True)
        os.chown(p, UID, GID)
        os.chmod(p, 0o700)
    fd, tmp = tempfile.mkstemp(prefix=".password-", dir=cdir)
    with os.fdopen(fd, "w") as f:
        f.write(stored + "\n")
    os.chown(tmp, UID, GID)
    os.chmod(tmp, 0o600)
    os.replace(tmp, cdir + "/password.hash")
    print("password set")


def run(c):
    init_data(c)
    if not os.path.isfile(c["DATA_DIR"] + "/ui/.config/gpu-tuner/password.hash"):
        sys.exit("No WebUI password set. Set one on Settings > GPU Tuner first.")
    os.makedirs(RUN, exist_ok=True)
    os.chmod(RUN, 0o711)
    env = {**os.environ, "PYTHONPATH": ROOT + "/vendor", "PYTHONUNBUFFERED": "1"}
    kids = []
    stopping = []

    def stop(*_):
        stopping.append(1)
        for k in kids:
            if k.poll() is None:
                k.terminate()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    def as_nobody():
        os.setgroups([])
        os.setgid(GID)
        os.setuid(UID)

    try:
        kids.append(subprocess.Popen(daemon_cmd(c), env=env))
        for _ in range(100):
            if os.path.exists(SOCK) or kids[0].poll() is not None or stopping:
                break
            time.sleep(0.1)
        if not os.path.exists(SOCK):
            sys.exit("daemon socket did not appear (is the NVIDIA driver plugin installed?)")
        ip = lan_ip(c)
        ui_env = {**env, "HOME": c["DATA_DIR"] + "/ui", "GPU_TUNER_BIND": "0.0.0.0", "GPU_TUNER_LAN_IP": ip}
        kids.append(subprocess.Popen([sys.executable, ROOT + "/gpu-tuner", "serve", "--socket", SOCK,
                                      "--port", c["PORT"]], env=ui_env, preexec_fn=as_nobody))
        print(f"WebUI http://{ip}:{c['PORT']}", flush=True)
        while not stopping and all(k.poll() is None for k in kids):
            time.sleep(0.5)
    finally:
        stop()
        for k in kids:
            try:
                k.wait(timeout=10)
            except subprocess.TimeoutExpired:
                k.kill()
        subprocess.run(daemon_cmd(c, "--restore-fans"), env=env, timeout=15)


if __name__ == "__main__":
    c = cfg()
    if sys.argv[1:] == ["--set-password"]:
        set_password(c)
    elif not sys.argv[1:]:
        run(c)
    else:
        sys.exit("usage: supervisor.py [--set-password]")
