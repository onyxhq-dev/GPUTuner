//! gputuner: the root service behind the GPUTuner Unraid plugin.
//!
//!   gputuner run [--fake] [--data DIR] [--run DIR] [--cfg FILE]   run the daemon + sampler
//!   gputuner restore-fans [--fake]                                hand every fan back to the driver
//!   gputuner version

mod daemon;
mod fake;
mod hw;
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
    data: PathBuf,
    run: PathBuf,
    cfg: PathBuf,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        fake: false,
        data: "/boot/config/plugins/gputuner/data".into(),
        run: "/run/gpu-tuner".into(),
        cfg: "/boot/config/plugins/gputuner/gputuner.cfg".into(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--fake" => o.fake = true,
            "--data" => o.data = val("--data")?.into(),
            "--run" => o.run = val("--run")?.into(),
            "--cfg" => o.cfg = val("--cfg")?.into(),
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(o)
}

fn open_hw(fake: bool) -> Result<Arc<dyn Hw>, String> {
    if fake {
        Ok(Arc::new(fake::FakeHw::demo()))
    } else {
        Ok(Arc::new(nvml_real::RealHw::new()?))
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

    let hw = open_hw(o.fake)?;
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => parse(&args[1..]).and_then(run),
        Some("restore-fans") => parse(&args[1..]).and_then(|o| {
            // A stop hook must not fail: with no driver there is nothing to hand back.
            match open_hw(o.fake) {
                Ok(hw) => daemon::restore_all(hw.as_ref()),
                Err(e) => util::log(&format!("restore-fans: {e}; nothing to do")),
            }
            Ok(())
        }),
        Some("version") | Some("--version") => {
            println!("gputuner {}", daemon::VERSION);
            Ok(())
        }
        _ => Err("usage: gputuner run [--fake] [--data DIR] [--run DIR] [--cfg FILE] | restore-fans | version".into()),
    };
    if let Err(e) = result {
        eprintln!("gputuner: {e}");
        std::process::exit(2);
    }
}
