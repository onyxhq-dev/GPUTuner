#!/usr/bin/env python3
# One-shot JSON helper called by api.php (runs as root, so the daemon socket accepts it).
#   api.py state          -> readings + daemon status for every GPU
#   api.py apply < req    -> forward one whitelisted change request to gpu-tunerd, print its reply
import json, sys

sys.path[:0] = ["/usr/local/lib/gputuner", "/usr/local/lib/gputuner/vendor"]
SOCK = "/run/gpu-tuner/control.sock"


def main(argv):
    from gpu_tuner.node import Collector
    from gpu_tuner.nvml import Nvml
    nv = Nvml()
    c = Collector(nv, SOCK)
    if argv == ["state"]:
        status, err = c.daemon_status()
        return {"ok": True, "driver": nv.driver, "static": [g.static() for g in nv.gpus],
                "live": c.sample(), "status": status, "daemon_error": err}
    if argv == ["apply"]:
        return c.apply(json.loads(sys.stdin.read(65536)))
    return {"ok": False, "error": "usage: api.py state|apply"}


if __name__ == "__main__":
    try:
        out = main(sys.argv[1:])
    except Exception as e:  # NVML missing, bad JSON, ...: report, never traceback into the page
        out = {"ok": False, "error": f"{type(e).__name__}: {e}"[:300]}
    print(json.dumps(out, allow_nan=False, default=str))
