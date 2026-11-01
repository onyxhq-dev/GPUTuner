//! The once-a-second sampler. It keeps three things for the settings page, all outside the daemon's
//! request path:
//!   * `live.json`     the latest reading of every card (RAM, replaced each second)
//!   * `history.jsonl` a row every 5 seconds, the last 6 hours (RAM)
//!   * saved history   per-minute averages, flushed to the flash drive in batches, 30 days kept
//!
//! A history row is {"t": unix, "g": {uuid: [temp, fan%, power W, limit W, clock MHz, util %,
//! vram used MiB, vram total MiB]}}, the same layout the page's API reads.

use crate::hw::{Hw, Sample};
use crate::util::{log, unix_now, ymd};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const HISTORY_EVERY: u64 = 5;
const KEEP_RAM_ROWS: usize = 4320;
const FLUSH_EVERY_ROWS: u64 = 120;
const RETENTION_DAYS: i64 = 30;

#[derive(Clone)]
pub struct Paths {
    pub run: PathBuf,
    pub data: PathBuf,
    pub cfg: PathBuf,
}

impl Paths {
    fn live(&self) -> PathBuf {
        self.run.join("live.json")
    }
    fn ram_history(&self) -> PathBuf {
        self.run.join("history.jsonl")
    }
    pub fn saved(&self) -> PathBuf {
        self.data.join("history")
    }
}

fn num(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

fn opt(x: Option<f64>) -> Value {
    x.map(num).unwrap_or(Value::Null)
}

fn row_values(s: &Sample) -> Value {
    let fan = s.fans.iter().flatten().copied().max();
    json!([
        opt(s.temp.map(|v| v as f64)),
        opt(fan.map(|v| v as f64)),
        opt(s.power_w),
        opt(s.power_limit_w.map(|v| v as f64)),
        opt(s.clock_mhz.map(|v| v as f64)),
        opt(s.util.map(|v| v as f64)),
        opt(s.vram_used_mib.map(|v| v as f64)),
        opt(s.vram_total_mib.map(|v| v as f64)),
    ])
}

/// Mean of each column over a minute of rows; the VRAM total (last column) keeps its latest value.
pub fn average(rows: &[Value]) -> Value {
    let last = rows.last().and_then(|r| r["g"].as_object()).cloned().unwrap_or_default();
    let mut out = Map::new();
    for uuid in last.keys() {
        let cols: Vec<&Vec<Value>> = rows.iter().filter_map(|r| r["g"][uuid].as_array()).collect();
        let vals: Vec<Value> = (0..8)
            .map(|i| {
                let xs: Vec<f64> = cols.iter().filter_map(|c| c.get(i).and_then(|v| v.as_f64())).collect();
                match xs.last() {
                    None => Value::Null,
                    Some(&l) if i == 7 => num(l),
                    Some(_) => num(((xs.iter().sum::<f64>() / xs.len() as f64) * 10.0).round() / 10.0),
                }
            })
            .collect();
        out.insert(uuid.clone(), Value::Array(vals));
    }
    json!({"t": rows.last().map(|r| r["t"].clone()).unwrap_or(Value::Null), "g": out})
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

fn save_enabled(cfg: &Path) -> bool {
    fs::read_to_string(cfg).map(|t| !t.contains("HISTORY=\"disable\"")).unwrap_or(true)
}

/// Append the pending per-minute rows to the daily files on flash, then drop files past retention.
pub fn flush_history(pending: &mut Vec<Value>, paths: &Paths, now: i64) {
    let rows = std::mem::take(pending);
    if rows.is_empty() || !save_enabled(&paths.cfg) {
        return;
    }
    let dir = paths.saved();
    let result = (|| -> std::io::Result<()> {
        fs::create_dir_all(&dir)?;
        let mut days: BTreeMap<String, String> = BTreeMap::new();
        for r in &rows {
            let day = ymd(r["t"].as_i64().unwrap_or(0));
            let e = days.entry(day).or_default();
            e.push_str(&r.to_string());
            e.push('\n');
        }
        for (day, text) in days {
            let mut f = fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{day}.jsonl")))?;
            f.write_all(text.as_bytes())?;
            f.flush()?;
            f.sync_all()?;
        }
        let cutoff = ymd(now - RETENTION_DAYS * 86400);
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.len() >= 10 && name[..10] < *cutoff {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        log(&format!("could not save history to {}: {e}", dir.display()));
    }
}

fn trim_ram_history(path: &Path) {
    let Ok(text) = fs::read_to_string(path) else { return };
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > KEEP_RAM_ROWS {
        let keep = lines[lines.len() - KEEP_RAM_ROWS..].join("\n") + "\n";
        let _ = write_atomic(path, keep.as_bytes());
    }
}

pub fn run(hw: Arc<dyn Hw>, paths: Paths, stop: Arc<AtomicBool>) {
    let mut minute: Vec<Value> = vec![];
    let mut pending: Vec<Value> = vec![];
    let mut tick: u64 = 0;
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let mut samples = Map::new();
        let mut row = Map::new();
        for (i, g) in hw.gpus().iter().enumerate() {
            let s = hw.sample(i);
            row.insert(g.uuid.clone(), row_values(&s));
            samples.insert(g.uuid.clone(), serde_json::to_value(&s).unwrap_or(Value::Null));
        }
        let now = unix_now();
        let _ = write_atomic(&paths.live(), json!({"t": now, "gpus": samples}).to_string().as_bytes());
        tick += 1;
        if tick % HISTORY_EVERY == 0 {
            let rec = json!({"t": now, "g": row});
            if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(paths.ram_history()) {
                let _ = writeln!(f, "{rec}");
            }
            minute.push(rec);
            if minute.len() >= 12 {
                pending.push(average(&minute));
                minute.clear();
            }
            if tick % (HISTORY_EVERY * FLUSH_EVERY_ROWS) == 0 {
                trim_ram_history(&paths.ram_history());
                flush_history(&mut pending, &paths, now);
            }
        }
        next += Duration::from_secs(1);
        while !stop.load(Ordering::Relaxed) {
            let left = next.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
    }
    if !minute.is_empty() {
        pending.push(average(&minute));
    }
    flush_history(&mut pending, &paths, unix_now());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeHw;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gputuner-sampler-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn paths() -> Paths {
        let d = tmp();
        let p = Paths { run: d.join("run"), data: d.join("data"), cfg: d.join("gputuner.cfg") };
        fs::create_dir_all(&p.run).unwrap();
        p
    }

    fn row(t: i64, temp: f64, total: f64) -> Value {
        json!({"t": t, "g": {"GPU-a": [temp, 50, 100.5, 220, 1500, 40, 1000, total]}})
    }

    #[test]
    fn average_means_the_columns_and_keeps_the_latest_total() {
        let a = average(&[row(60, 40.0, 8192.0), row(120, 60.0, 8000.0)]);
        assert_eq!(a["t"], json!(120));
        assert_eq!(a["g"]["GPU-a"], json!([50, 50, 100.5, 220, 1500, 40, 1000, 8000]));
    }

    #[test]
    fn average_ignores_missing_readings_and_keeps_null_when_all_are_missing() {
        let a = average(&[
            json!({"t": 1, "g": {"G": [null, 50, null, 220, 1500, 40, 1000, 8192]}}),
            json!({"t": 2, "g": {"G": [70, 60, null, 220, 1500, 40, 1000, 8192]}}),
        ]);
        assert_eq!(a["g"]["G"], json!([70, 55, null, 220, 1500, 40, 1000, 8192]));
    }

    #[test]
    fn flush_writes_daily_files_in_order_and_prunes_old_ones() {
        let p = paths();
        let now = 1_793_493_300; // 2026-11-01
        fs::create_dir_all(p.saved()).unwrap();
        fs::write(p.saved().join(format!("{}.jsonl", ymd(now - 40 * 86400))), "{}\n").unwrap();
        fs::write(p.saved().join(format!("{}.jsonl", ymd(now - 29 * 86400))), "{}\n").unwrap();
        let mut pending = vec![row(now - 86400 - 60, 40.0, 8192.0), row(now - 60, 41.0, 8192.0), row(now, 42.0, 8192.0)];
        flush_history(&mut pending, &p, now);
        assert!(pending.is_empty());
        let names: Vec<String> =
            fs::read_dir(p.saved()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        assert!(!names.contains(&format!("{}.jsonl", ymd(now - 40 * 86400))), "40-day-old file pruned: {names:?}");
        assert!(names.contains(&format!("{}.jsonl", ymd(now - 29 * 86400))), "29-day-old file kept: {names:?}");
        let today = fs::read_to_string(p.saved().join("2026-11-01.jsonl")).unwrap();
        assert_eq!(today.lines().count(), 2);
        let yesterday = fs::read_to_string(p.saved().join("2026-10-31.jsonl")).unwrap();
        assert_eq!(yesterday.lines().count(), 1);
        flush_history(&mut vec![row(now + 60, 43.0, 8192.0)], &p, now + 60);
        assert_eq!(fs::read_to_string(p.saved().join("2026-11-01.jsonl")).unwrap().lines().count(), 3, "appends");
    }

    #[test]
    fn turning_save_history_off_stops_writing_and_drops_the_batch() {
        let p = paths();
        fs::write(&p.cfg, "SERVICE=\"enable\"\nHISTORY=\"disable\"\n").unwrap();
        let mut pending = vec![row(1_793_493_300, 40.0, 8192.0)];
        flush_history(&mut pending, &p, 1_793_493_300);
        assert!(pending.is_empty());
        assert!(!p.saved().exists());
        fs::write(&p.cfg, "SERVICE=\"enable\"\nHISTORY=\"enable\"\n").unwrap();
        flush_history(&mut vec![row(1_793_493_300, 40.0, 8192.0)], &p, 1_793_493_300);
        assert!(p.saved().exists());
    }

    #[test]
    fn the_sampler_writes_live_and_stops_promptly() {
        let p = paths();
        let hw: Arc<dyn Hw> = Arc::new(FakeHw::pair());
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, p2) = (stop.clone(), p.clone());
        let t = std::thread::spawn(move || run(hw, p2, s2));
        std::thread::sleep(Duration::from_millis(1500));
        let live: Value = serde_json::from_str(&fs::read_to_string(p.live()).unwrap()).unwrap();
        assert!(live["t"].is_number());
        assert_eq!(live["gpus"]["GPU-maxq-0000"]["temp"], json!(40));
        assert_eq!(live["gpus"]["GPU-ws-0000"]["fans"], json!([45, 45]));
        let t0 = Instant::now();
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert!(t0.elapsed() < Duration::from_secs(1), "stopped within a second");
    }
}
