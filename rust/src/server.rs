//! The control socket: one JSON object per line in, one JSON object per line out. The socket is
//! created mode 0600 in a root-only directory, so only root (the Unraid web UI's PHP) can use it.

use crate::daemon::Daemon;
use crate::util::log;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_REQUEST: usize = 64 * 1024;
const READ_DEADLINE: Duration = Duration::from_secs(2);

fn read_line(stream: &mut UnixStream) -> Result<Vec<u8>, String> {
    stream.set_read_timeout(Some(Duration::from_millis(200))).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + READ_DEADLINE;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.contains(&b'\n') && buf.len() <= MAX_REQUEST && Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    if buf.len() > MAX_REQUEST {
        return Err("request too large".into());
    }
    Ok(buf.split(|&b| b == b'\n').next().unwrap_or(&[]).to_vec())
}

fn serve_one(mut stream: UnixStream, daemon: &mut Daemon) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let resp = match read_line(&mut stream) {
        Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
        Ok(line) => match serde_json::from_slice::<Value>(&line) {
            Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
            Ok(req) => {
                let r = catch_unwind(AssertUnwindSafe(|| daemon.handle(&req))).unwrap_or_else(|_| {
                    log("FAULT handling a request: internal error");
                    json!({"ok": false, "error": "internal error; see the service log"})
                });
                let op = req.get("op").and_then(|o| o.as_str()).unwrap_or("");
                if op != "status" && op != "state" {
                    let detail: serde_json::Map<String, Value> = ["op", "uuid", "watts", "mode", "manual_pct", "mhz", "confirm_override"]
                        .iter()
                        .filter_map(|k| req.get(*k).map(|v| (k.to_string(), v.clone())))
                        .collect();
                    log(&format!(
                        "audit {} -> {}",
                        Value::Object(detail),
                        if r["ok"] == json!(true) { "ok".to_string() } else { format!("REJECTED: {}", r["error"]) }
                    ));
                }
                r
            }
        },
    };
    let mut out = resp.to_string();
    out.push('\n');
    if let Err(e) = stream.write_all(out.as_bytes()) {
        log(&format!("client error: {e}"));
    }
}

/// Run the fan loop and answer requests until `stop` is set.
pub fn serve(daemon: &mut Daemon, listener: &UnixListener, stop: &AtomicBool) {
    let t0 = Instant::now();
    let interval = daemon.interval_s();
    let mut next_tick = 0.0_f64;
    let mut pfd = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    while !stop.load(Ordering::Relaxed) {
        let now = t0.elapsed().as_secs_f64();
        if now >= next_tick {
            daemon.tick(now);
            next_tick = now + interval;
        }
        let wait_ms = ((next_tick - t0.elapsed().as_secs_f64()).clamp(0.05, 0.5) * 1000.0) as i32;
        pfd.revents = 0;
        // SAFETY: pfd points to one valid pollfd for the duration of the call.
        let n = unsafe { libc::poll(&mut pfd, 1, wait_ms) };
        if n > 0 && pfd.revents & libc::POLLIN != 0 {
            match listener.accept() {
                Ok((stream, _)) => serve_one(stream, daemon),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => log(&format!("accept failed: {e}")),
            }
        }
    }
}
