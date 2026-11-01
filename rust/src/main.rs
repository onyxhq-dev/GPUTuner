//! gputuner: the root service behind the GPUTuner Unraid plugin.
//!
//!   gputuner run [--fake] [--no-nvidia] [--sysfs DIR] [--data DIR] [--run DIR] [--cfg FILE]
//!                                                   run the daemon + sampler
//!   gputuner probe [--no-nvidia] [--sysfs DIR]      read-only: list the GPUs and what can be controlled
//!   gputuner restore-fans [--fake]                  hand every fan back to the driver
//!   gputuner version

mod amd;
mod daemon;
mod fake;
mod hw;
mod multi;
mod nvml_real;
mod safety;
mod sampler;
#[cfg(unix)]
mod server;
mod util;

use hw::Hw;
use std::path::PathBuf;
use std::sync::Arc;

struct Opts {
    fake: bool,
    no_nvidia: bool,
    sysfs: PathBuf,
    data: PathBuf,
    run: PathBuf,
    cfg: PathBuf,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        fake: false,
        no_nvidia: false,
        sysfs: "/sys".into(),
        data: "/boot/config/plugins/gputuner/data".into(),
        run: "/run/gpu-tuner".into(),
        cfg: "/boot/config/plugins/gputuner/gputuner.cfg".into(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--fake" => o.fake = true,
            "--no-nvidia" => o.no_nvidia = true,
            "--sysfs" => o.sysfs = val("--sysfs")?.into(),
            "--data" => o.data = val("--data")?.into(),
            "--run" => o.run = val("--run")?.into(),
            "--cfg" => o.cfg = val("--cfg")?.into(),
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(o)
}

/// Find every supported GPU. NVIDIA through NVML, AMD (experimental) through amdgpu's sysfs. With
/// only one vendor present the backend is used directly, exactly as before AMD support existed.
fn open_hw(o: &Opts) -> Result<Arc<dyn Hw>, String> {
    if o.fake {
        return Ok(Arc::new(fake::FakeHw::demo()));
    }
    let mut parts: Vec<Arc<dyn Hw>> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    if !o.no_nvidia {
        match nvml_real::RealHw::new() {
            Ok(h) if !h.gpus().is_empty() => parts.push(Arc::new(h)),
            Ok(_) => notes.push("the NVIDIA driver is loaded but reports no GPU".into()),
            Err(e) => notes.push(e),
        }
    }
    if let Some(a) = amd::AmdHw::discover(&o.sysfs, std::path::Path::new("/usr/share/hwdata/pci.ids")) {
        util::log(&format!(
            "WARNING AMD GPU support is EXPERIMENTAL (in testing): {} card(s) found. Use at your own risk.",
            a.gpus().len()
        ));
        parts.push(Arc::new(a));
    }
    match parts.len() {
        0 => Err(format!("no supported GPU found ({})", if notes.is_empty() { "no NVIDIA or AMD GPU".to_string() } else { notes.join("; ") })),
        1 => Ok(parts.remove(0)),
        _ => Ok(Arc::new(multi::Multi::new(parts))),
    }
}

#[cfg(unix)]
fn run(o: Opts) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::AtomicBool;
    use util::log;

    let config_path = o.data.join("config/config.json");
    let state_path = o.data.join("state/state.json");
    for sub in ["config", "state"] {
        std::fs::create_dir_all(o.data.join(sub)).map_err(|e| format!("cannot create {}: {e}", o.data.display()))?;
    }
    if !config_path.exists() {
        let seed = r#"{"allowed_uid": null, "gpu_budget_w": null, "interval_s": 2.0}"#;
        std::fs::write(&config_path, seed).map_err(|e| format!("cannot write {}: {e}", config_path.display()))?;
    }
    std::fs::create_dir_all(&o.run).map_err(|e| format!("cannot create {}: {e}", o.run.display()))?;
    let _ = std::fs::set_permissions(&o.run, std::fs::Permissions::from_mode(0o700));

    let hw = open_hw(&o)?;
    let cfg = daemon::load_config(&config_path)?;
    let mut d = daemon::Daemon::new(hw.clone(), cfg, state_path, Some(config_path));
    log(&format!(
        "gputuner {} driver {}{} budget {}",
        daemon::VERSION,
        hw.driver(),
        if o.fake { " (fake GPU)" } else { "" },
        d.budget.map(|b| format!("{b} W")).unwrap_or_else(|| "none".into())
    ));
    for (i, c) in d.cards.iter().enumerate() {
        log(&format!(
            "  gpu{i} {} {} fans={} settable={}",
            c.info.name,
            c.info.uuid,
            c.info.nfans,
            c.range.map(|r| format!("{}-{} W", r.0, r.1)).unwrap_or_else(|| "no".into())
        ));
    }
    d.load_state();
    d.apply_startup();
    if let Err(e) = d.save_state() {
        log(&format!("could not save state: {e}"));
    }

    let sock = o.run.join("control.sock");
    let _ = std::fs::remove_file(&sock);
    // SAFETY: umask only changes this process's file-creation mask.
    let old = unsafe { libc::umask(0o177) };
    let bound = UnixListener::bind(&sock);
    unsafe { libc::umask(old) };
    let listener = bound.map_err(|e| format!("cannot bind {}: {e}", sock.display()))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;

    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(sig, stop.clone()).map_err(|e| e.to_string())?;
    }
    let paths = sampler::Paths { run: o.run.clone(), data: o.data.clone(), cfg: o.cfg.clone() };
    let sampler_thread = {
        let (hw, stop) = (hw.clone(), stop.clone());
        std::thread::spawn(move || sampler::run(hw, paths, stop))
    };

    server::serve(&mut d, &listener, &stop);

    d.restore_fans();
    let _ = std::fs::remove_file(&sock);
    let _ = sampler_thread.join();
    Ok(())
}

#[cfg(not(unix))]
fn run(_o: Opts) -> Result<(), String> {
    Err("gputuner run needs Linux (Unix sockets)".into())
}

/// Read-only: say what would be controlled on this machine. Nothing is written to any GPU.
fn probe(o: Opts) -> Result<(), String> {
    println!("gputuner {} probe (read-only, nothing is written to any GPU)", daemon::VERSION);
    match open_hw(&o) {
        Err(e) => println!("\nNo supported GPU: {e}"),
        Ok(hw) => {
            println!("\ndriver: {}", hw.driver());
            for (i, g) in hw.gpus().iter().enumerate() {
                let s = hw.sample(i);
                let w = |v: Option<i64>| v.map(|x| x.to_string()).unwrap_or_else(|| "-".into());
                println!("\nGPU {i} [{}] {}  ({}, {})", g.vendor, g.name, g.uuid, g.pci);
                println!(
                    "  power limit: now {} W, settable {} (default {} W)",
                    w(s.power_limit_w),
                    match (g.power_min_w, g.power_max_w) {
                        (Some(a), Some(b)) => format!("{a}-{b} W"),
                        _ => "not available".into(),
                    },
                    w(g.power_default_w)
                );
                println!("  fans: {} controllable, now {:?} %", g.nfans, s.fans);
                println!("  clock cap: {}", if g.clocks.is_empty() { "not available" } else { "available" });
                println!("  slowdown temperature: {} C", w(g.slowdown()));
                println!(
                    "  reading: {} C, {} W, {} MHz, util {} %, vram {}/{} MiB",
                    w(s.temp),
                    s.power_w.map(|x| x.to_string()).unwrap_or_else(|| "-".into()),
                    w(s.clock_mhz),
                    w(s.util),
                    w(s.vram_used_mib),
                    w(s.vram_total_mib)
                );
            }
        }
    }
    println!("\nAMD sysfs report ({}):\n{}", o.sysfs.display(), amd::diagnose(&o.sysfs));
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => parse(&args[1..]).and_then(run),
        Some("restore-fans") => parse(&args[1..]).and_then(|o| {
            // A stop hook must not fail: with no driver there is nothing to hand back.
            match open_hw(&o) {
                Ok(hw) => daemon::restore_all(hw.as_ref()),
                Err(e) => util::log(&format!("restore-fans: {e}; nothing to do")),
            }
            Ok(())
        }),
        Some("probe") => parse(&args[1..]).and_then(probe),
        Some("version") | Some("--version") => {
            println!("gputuner {}", daemon::VERSION);
            Ok(())
        }
        _ => Err("usage: gputuner run [--fake] [--no-nvidia] [--sysfs DIR] [--data DIR] [--run DIR] [--cfg FILE] | probe | restore-fans | version".into()),
    };
    if let Err(e) = result {
        eprintln!("gputuner: {e}");
        std::process::exit(2);
    }
}
