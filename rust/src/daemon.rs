//! Ported from gpu_tuner/daemon.py of github.com/willboyden/gpu-tuner (MIT, see LICENSE-gpu-tuner).
//!
//! The decision-maker: loads and validates saved settings, applies them to the hardware, runs the
//! fan loop, re-asserts power limits, and answers requests. Everything that can write to a GPU is
//! checked against `safety` first, and every driver write is read back where it can be.

use crate::hw::*;
use crate::safety::{self, Profile, SafetyErr};
use crate::util::log;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub const FAULT_RETRY_S: f64 = 30.0;
pub const FAN_REASSERT_S: f64 = 30.0;
pub const POWER_CHECK_S: f64 = 10.0;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── config ─────────────────────────────────────────────────────────────────────────────────────
pub struct Config {
    pub gpu_budget_w: Option<i64>,
    pub interval_s: f64,
    raw: Map<String, Value>,
}

impl Default for Config {
    fn default() -> Self {
        Config { gpu_budget_w: Some(safety::DEFAULT_GPU_BUDGET_W), interval_s: 2.0, raw: Map::new() }
    }
}

pub fn load_config(path: &Path) -> Result<Config, String> {
    let mut cfg = Config::default();
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log(&format!("config {} not found: defaults ({} W budget)", path.display(), safety::DEFAULT_GPU_BUDGET_W));
            return Ok(cfg);
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let raw: Value = serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    let obj = raw.as_object().ok_or_else(|| format!("{} must be a JSON object", path.display()))?;
    if let Some(uid) = obj.get("allowed_uid").filter(|v| !v.is_null()) {
        if uid.as_u64().is_none() {
            return Err(format!("allowed_uid in {} must be a uid", path.display()));
        }
    }
    match obj.get("gpu_budget_w") {
        Some(Value::Null) => cfg.gpu_budget_w = None,
        Some(v) => match v.as_i64() {
            Some(b) if b > 0 => cfg.gpu_budget_w = Some(b),
            _ => return Err(format!("gpu_budget_w in {} must be a positive whole number of watts", path.display())),
        },
        None => {}
    }
    if let Some(v) = obj.get("interval_s").filter(|v| !v.is_null()) {
        match v.as_f64() {
            Some(i) if (1.0..=10.0).contains(&i) => cfg.interval_s = i,
            _ => return Err(format!("interval_s in {} must be 1-10", path.display())),
        }
    }
    cfg.raw = obj.clone();
    Ok(cfg)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.flush()?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

// ── settings ───────────────────────────────────────────────────────────────────────────────────
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FanSettings {
    pub mode: String,
    pub curve: Vec<[i64; 2]>,
    pub manual_pct: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub power_w: Option<i64>,
    pub clock_cap_mhz: Option<i64>,
    pub fan: FanSettings,
}

#[derive(Clone, Debug)]
pub struct Rt {
    pub prev: Option<(i64, i64)>,
    pub fan_pct: Option<i64>,
    pub floor_active: bool,
    pub fault: Option<String>,
    pub retry_at: f64,
    pub auto_applied: bool,
    pub temp: Option<i64>,
    pub written_at: f64,
}

impl Rt {
    fn fresh() -> Rt {
        Rt { prev: None, fan_pct: None, floor_active: false, fault: None, retry_at: 0.0, auto_applied: false, temp: None, written_at: 0.0 }
    }
}

pub struct Card {
    pub info: GpuInfo,
    pub profile: Option<&'static Profile>,
    pub range: Option<(i64, i64)>,
    pub settings: Settings,
    pub rt: Rt,
}

fn full_by(g: &GpuInfo) -> i64 {
    safety::safety_floor_for(g.slowdown()).1
}

fn default_curve_for(g: &GpuInfo) -> Vec<[i64; 2]> {
    safety::default_curve(full_by(g)).iter().map(|&(t, p)| [t, p]).collect()
}

fn default_fan(g: &GpuInfo) -> FanSettings {
    FanSettings { mode: "auto".into(), curve: default_curve_for(g), manual_pct: 60 }
}

/// An operation failed: a rule of the safety envelope, or the driver itself.
#[derive(Debug)]
enum OpErr {
    Safety(SafetyErr),
    Driver(String),
}

impl From<SafetyErr> for OpErr {
    fn from(e: SafetyErr) -> Self {
        OpErr::Safety(e)
    }
}

fn driver<E: std::fmt::Display>(e: E) -> OpErr {
    OpErr::Driver(e.to_string())
}

// ── the daemon ─────────────────────────────────────────────────────────────────────────────────
pub struct Daemon {
    hw: Arc<dyn Hw>,
    cfg: Config,
    pub state_path: PathBuf,
    pub config_path: Option<PathBuf>,
    pub budget: Option<i64>,
    pub warnings: Vec<String>,
    pub cards: Vec<Card>,
    next_power_check: f64,
    t0: Instant,
}

impl Daemon {
    pub fn new(hw: Arc<dyn Hw>, cfg: Config, state_path: PathBuf, config_path: Option<PathBuf>) -> Daemon {
        let cards = hw
            .gpus()
            .iter()
            .map(|g| {
                let profile = safety::profile_for(&g.name);
                let range = safety::power_range(profile, g.power_min_w, g.power_max_w, g.power_default_w);
                Card {
                    info: g.clone(),
                    profile,
                    range,
                    settings: Settings { power_w: None, clock_cap_mhz: None, fan: default_fan(g) },
                    rt: Rt::fresh(),
                }
            })
            .collect();
        Daemon {
            budget: cfg.gpu_budget_w,
            hw,
            cfg,
            state_path,
            config_path,
            warnings: vec![],
            cards,
            next_power_check: 0.0,
            t0: Instant::now(),
        }
    }

    /// Monotonic seconds, offset so it is always far ahead of the small values tests pass to tick().
    fn now(&self) -> f64 {
        1.0e6 + self.t0.elapsed().as_secs_f64()
    }

    pub fn interval_s(&self) -> f64 {
        self.cfg.interval_s
    }

    fn warn(&mut self, msg: String) {
        log(&format!("WARNING {msg}"));
        self.warnings.push(msg);
    }

    // ── settings: load, adopt, persist ────────────────────────────────────────────────────────
    pub fn baseline_power(&self, i: usize) -> Option<i64> {
        let c = &self.cards[i];
        let rng = c.range?;
        let want = match c.profile {
            Some(p) => p.baseline_power_w,
            None => c.info.power_default_w.unwrap_or(0),
        };
        Some(rng.0.max(rng.1.min(want)))
    }

    /// One card's settings from the state file, each field re-validated: a stale file (budget
    /// lowered, card swapped) must not bypass the checks.
    fn load_one(&self, i: usize, saved: &Value, warns: &mut Vec<String>) -> Settings {
        let c = &self.cards[i];
        let g = &c.info;
        let mut s = Settings { power_w: None, clock_cap_mhz: None, fan: default_fan(g) };
        let saved_obj = saved.as_object();
        if let Some(rng) = c.range {
            let live = self.hw.power_limit_w(i);
            let want = match saved_obj.and_then(|o| o.get("power_w")) {
                Some(v) => v.clone(),
                None => live.map(Value::from).unwrap_or(Value::Null),
            };
            let checked = safety::as_int(&want, "power limit").and_then(|w| {
                if rng.0 <= w && w <= rng.1 {
                    Ok(w)
                } else {
                    Err(SafetyErr::Msg(format!("{w} W is outside {}-{} W", rng.0, rng.1)))
                }
            });
            match checked {
                Ok(w) => s.power_w = Some(w),
                Err(e) => {
                    s.power_w = self.baseline_power(i);
                    warns.push(format!(
                        "{}: saved power limit rejected ({e}); using {} W",
                        g.name,
                        s.power_w.map(|w| w.to_string()).unwrap_or_else(|| "?".into())
                    ));
                }
            }
        }
        if g.nfans > 0 {
            if let Some(fan) = saved_obj.and_then(|o| o.get("fan")).and_then(|f| f.as_object()) {
                let attempt = (|| -> Result<FanSettings, SafetyErr> {
                    let mut f2 = s.fan.clone();
                    if let Some(m) = fan.get("mode").and_then(|m| m.as_str()) {
                        if matches!(m, "curve" | "manual" | "auto") {
                            f2.mode = m.to_string();
                        }
                    }
                    if let Some(c) = fan.get("curve") {
                        f2.curve = safety::validate_curve(c, full_by(g), g.fan_min)?;
                    }
                    if let Some(p) = fan.get("manual_pct") {
                        f2.manual_pct = safety::validate_manual(p, g.fan_min)?;
                    }
                    Ok(f2)
                })();
                match attempt {
                    Ok(f2) => s.fan = f2,
                    Err(e) => {
                        s.fan = default_fan(g);
                        warns.push(format!(
                            "{}: saved fan settings rejected ({e}); using this card's default curve",
                            g.name
                        ));
                    }
                }
            }
        }
        let cap = saved_obj.and_then(|o| o.get("clock_cap_mhz")).cloned().unwrap_or(Value::Null);
        match safety::validate_clock_cap(&cap, &g.clocks) {
            Ok(v) => s.clock_cap_mhz = v,
            Err(e) => warns.push(format!("{}: saved clock cap rejected ({e}); cleared", g.name)),
        }
        s
    }

    pub fn load_state(&mut self) {
        let mut saved = Value::Null;
        match fs::read_to_string(&self.state_path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(doc) => {
                    if let Some(g) = doc.get("gpus").filter(|g| g.is_object()) {
                        saved = g.clone();
                    }
                }
                Err(e) => {
                    let m = format!("state file unreadable ({e}); starting from live limits and the lab curve");
                    self.warn(m);
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log("no saved state: adopting the power limits currently in force and the lab fan curve")
            }
            Err(e) => {
                let m = format!("state file unreadable ({e}); starting from live limits and the lab curve");
                self.warn(m);
            }
        }
        let mut warns = vec![];
        for i in 0..self.cards.len() {
            let uuid = self.cards[i].info.uuid.clone();
            let one = saved.get(&uuid).cloned().unwrap_or(Value::Null);
            self.cards[i].settings = self.load_one(i, &one, &mut warns);
        }
        for w in warns {
            self.warn(w);
        }
        let total: i64 = self.cards.iter().map(|c| c.settings.power_w.unwrap_or(0)).sum();
        if let Some(budget) = self.budget {
            if total > budget {
                for i in 0..self.cards.len() {
                    if self.cards[i].range.is_some() {
                        self.cards[i].settings.power_w = self.baseline_power(i);
                    }
                }
                let after: i64 = self.cards.iter().map(|c| c.settings.power_w.unwrap_or(0)).sum();
                if after > budget {
                    for c in self.cards.iter_mut() {
                        if let Some(r) = c.range {
                            c.settings.power_w = Some(r.0);
                        }
                    }
                }
                let now: i64 = self.cards.iter().map(|c| c.settings.power_w.unwrap_or(0)).sum();
                self.warn(format!("saved power limits totalled {total} W, over the {budget} W budget; fell back to {now} W"));
            }
        }
    }

    pub fn save_state(&self) -> std::io::Result<()> {
        let gpus: BTreeMap<&str, &Settings> = self.cards.iter().map(|c| (c.info.uuid.as_str(), &c.settings)).collect();
        let doc = json!({"version": 1, "gpus": gpus});
        write_atomic(&self.state_path, serde_json::to_string_pretty(&doc)?.as_bytes())
    }

    fn save_config(&mut self) {
        let Some(path) = self.config_path.clone() else { return };
        let mut raw = self.cfg.raw.clone();
        raw.entry("allowed_uid").or_insert(Value::Null);
        raw.insert("gpu_budget_w".into(), self.budget.map(Value::from).unwrap_or(Value::Null));
        raw.insert("interval_s".into(), json!(self.cfg.interval_s));
        if let Err(e) = write_atomic(&path, serde_json::to_string_pretty(&Value::Object(raw)).unwrap().as_bytes()) {
            log(&format!("could not save {}: {e}", path.display()));
        }
    }

    // ── applying settings to hardware ─────────────────────────────────────────────────────────
    /// Write, then READ BACK. Returns the limit actually in force.
    fn apply_power(&self, i: usize, watts: i64) -> Result<i64, String> {
        self.hw.set_power_limit_w(i, watts).map_err(|e| e.to_string())?;
        let live = self.hw.power_limit_w(i);
        if live != Some(watts) {
            return Err(format!(
                "driver accepted {watts} W but reports {} in force",
                live.map(|l| format!("{l} W")).unwrap_or_else(|| "nothing".into())
            ));
        }
        Ok(watts)
    }

    pub fn apply_startup(&mut self) {
        for i in 0..self.cards.len() {
            if let Err(e) = self.hw.set_persistence(i) {
                let name = self.cards[i].info.name.clone();
                if e.not_supported() {
                    log(&format!("{name}: persistence mode not supported by this GPU"));
                } else {
                    self.warn(format!("{name}: could not enable persistence mode: {e}"));
                }
            }
        }
        // Lower first, raise second: the combined cap never overshoots max(before, after).
        let mut todo: Vec<(usize, i64)> =
            (0..self.cards.len()).filter_map(|i| self.cards[i].settings.power_w.map(|w| (i, w))).collect();
        todo.sort_by_key(|&(i, w)| w - self.hw.power_limit_w(i).unwrap_or(0));
        for (i, watts) in todo {
            if self.hw.power_limit_w(i) == Some(watts) {
                continue;
            }
            let name = self.cards[i].info.name.clone();
            match self.apply_power(i, watts) {
                Ok(_) => log(&format!("{name}: power limit -> {watts} W")),
                Err(e) => self.warn(format!("{name}: could not apply {watts} W: {e}")),
            }
        }
        for i in 0..self.cards.len() {
            if let Some(cap) = self.cards[i].settings.clock_cap_mhz {
                let name = self.cards[i].info.name.clone();
                match self.hw.set_clock_cap(i, cap) {
                    Ok(()) => log(&format!("{name}: core clock capped at {cap} MHz")),
                    Err(e) => self.warn(format!("{name}: could not apply the {cap} MHz clock cap: {e}")),
                }
            }
        }
    }

    fn fan_fault(&mut self, i: usize, msg: &str, now: f64) {
        let handed = match self.hw.set_fan_auto(i) {
            Ok(()) => "fans handed back to the driver".to_string(),
            Err(e) => format!("and handing the fans back to the driver ALSO failed: {e}"),
        };
        let c = &mut self.cards[i];
        c.rt = Rt::fresh();
        c.rt.fault = Some(format!("{msg}; {handed}"));
        c.rt.retry_at = now + FAULT_RETRY_S;
        log(&format!("FAULT {}: {} (retry in {FAULT_RETRY_S}s)", c.info.name, c.rt.fault.as_deref().unwrap_or("")));
    }

    pub fn tick(&mut self, now: f64) {
        for i in 0..self.cards.len() {
            if self.cards[i].info.nfans == 0 || self.cards[i].rt.retry_at > now {
                continue;
            }
            let (name, mode) = (self.cards[i].info.name.clone(), self.cards[i].settings.fan.mode.clone());
            if mode == "auto" {
                if !self.cards[i].rt.auto_applied {
                    match self.hw.set_fan_auto(i) {
                        Ok(()) => {
                            self.cards[i].rt = Rt::fresh();
                            self.cards[i].rt.auto_applied = true;
                            log(&format!("{name}: fans -> driver default curve"));
                        }
                        Err(e) => self.fan_fault(i, &format!("could not hand fans to the driver: {e}"), now),
                    }
                }
                continue;
            }
            let temp = match self.hw.temp_c(i) {
                Ok(t) => t,
                Err(e) => {
                    self.fan_fault(i, &format!("temperature unreadable: {e}"), now);
                    continue;
                }
            };
            let (curve, manual, slowdown, fan_min) = {
                let c = &self.cards[i];
                (c.settings.fan.curve.clone(), c.settings.fan.manual_pct, c.info.slowdown(), c.info.fan_min)
            };
            let (target, floor_active) = match safety::fan_target(&mode, temp, &curve, manual, slowdown, fan_min) {
                Ok(t) => t,
                Err(e) => {
                    self.fan_fault(i, &format!("fan target: {e}"), now);
                    continue;
                }
            };
            let pct = safety::hysteresis(target, temp, self.cards[i].rt.prev);
            let changed = Some(pct) != self.cards[i].rt.fan_pct;
            if changed || now - self.cards[i].rt.written_at >= FAN_REASSERT_S {
                if let Err(e) = self.hw.set_fan_pct(i, pct) {
                    self.fan_fault(i, &format!("fan write refused: {e}"), now);
                    continue;
                }
                let rt = &mut self.cards[i].rt;
                rt.written_at = now;
                if changed {
                    log(&format!("{name}: {temp}C -> {pct}%{}", if floor_active { " (safety floor)" } else { "" }));
                    rt.prev = Some((pct, temp));
                }
            }
            let rt = &mut self.cards[i].rt;
            rt.fan_pct = Some(pct);
            rt.floor_active = floor_active;
            rt.temp = Some(temp);
            rt.fault = None;
        }
        if now >= self.next_power_check {
            self.next_power_check = now + POWER_CHECK_S;
            self.enforce_power();
        }
    }

    fn enforce_power(&mut self) {
        for i in 0..self.cards.len() {
            let want = self.cards[i].settings.power_w;
            let live = self.hw.power_limit_w(i);
            let (Some(want), Some(live)) = (want, live) else { continue };
            if live == want {
                continue;
            }
            let name = self.cards[i].info.name.clone();
            match self.apply_power(i, want) {
                Ok(_) => log(&format!(
                    "{name}: power limit had drifted to {live} W (changed outside gputuner); re-applied {want} W"
                )),
                Err(e) => log(&format!("FAULT {name}: limit is {live} W, wanted {want} W, re-apply failed: {e}")),
            }
        }
    }

    pub fn restore_fans(&self) {
        restore_all(self.hw.as_ref());
    }

    // ── requests ──────────────────────────────────────────────────────────────────────────────
    pub fn status(&self) -> Value {
        let used: i64 = self.cards.iter().map(|c| c.settings.power_w.unwrap_or(0)).sum();
        let gpus: Vec<Value> = (0..self.cards.len())
            .map(|i| {
                let c = &self.cards[i];
                let (floor, fb) = safety::safety_floor_for(c.info.slowdown());
                json!({
                    "uuid": c.info.uuid, "name": c.info.name, "fan_min": c.info.fan_min,
                    "profile": c.profile.map(|p| json!({
                        "label": p.label, "note": p.note,
                        "power_presets": p.presets.iter().map(|x| json!({"w": x.w, "label": x.label, "note": x.note})).collect::<Vec<_>>(),
                    })),
                    "baseline_power_w": self.baseline_power(i),
                    "power_range": c.range.map(|r| json!([r.0, r.1])),
                    "clock_range": c.info.clocks.iter().max().map(|m| json!([safety::CLOCK_CAP_MIN_MHZ, m])),
                    "settings": c.settings,
                    "fan": {"fan_pct": c.rt.fan_pct, "floor_active": c.rt.floor_active, "fault": c.rt.fault, "temp": c.rt.temp},
                    "full_by_c": fb,
                    "floor": floor.iter().map(|&(t, p)| json!([t, p])).collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut warnings: Vec<String> = self.warnings.iter().rev().take(10).rev().cloned().collect();
        if let Some(b) = self.budget {
            if used > b {
                warnings.push(format!("combined power is {used} W, over the {b} W budget"));
            }
        }
        let presets: Map<String, Value> = safety::CURVE_PRESETS
            .iter()
            .map(|p| {
                (
                    p.key.to_string(),
                    json!({"label": p.label, "note": p.note, "curve": p.curve.iter().map(|&(t, f)| json!([t, f])).collect::<Vec<_>>()}),
                )
            })
            .collect();
        json!({
            "ok": true, "version": VERSION, "dry_run": self.hw.dry_run(),
            "budget_w": self.budget, "budget_used_w": used,
            "power_settable": self.cards.iter().any(|c| c.range.is_some()),
            "curve_temp_min_c": safety::CURVE_TEMP_MIN_C, "curve_max_points": safety::CURVE_MAX_POINTS,
            "curve_presets": presets, "warnings": warnings, "gpus": gpus,
        })
    }

    /// Everything the settings page needs in one answer: hardware facts, live readings, status.
    pub fn ui_state(&self) -> Value {
        let live: Map<String, Value> = (0..self.cards.len())
            .map(|i| (self.cards[i].info.uuid.clone(), serde_json::to_value(self.hw.sample(i)).unwrap_or(Value::Null)))
            .collect();
        json!({
            "ok": true, "driver": self.hw.driver(),
            "static": self.cards.iter().map(|c| c.info.static_json()).collect::<Vec<_>>(),
            "live": live, "status": self.status(), "daemon_error": null,
        })
    }

    fn set_budget(&mut self, watts: &Value) -> Result<(), SafetyErr> {
        let w = safety::as_int(watts, "GPU budget")?;
        if w <= 0 {
            return Err(SafetyErr::Msg("the GPU budget must be a positive number of watts".into()));
        }
        let hw_max: i64 = self.cards.iter().filter_map(|c| c.range.map(|r| r.1)).sum();
        if hw_max == 0 {
            return Err(SafetyErr::Msg(
                "no card on this machine has a settable power limit, so there is nothing for a budget to limit".into(),
            ));
        }
        if w > hw_max {
            return Err(SafetyErr::Msg(format!(
                "{w} W is above {hw_max} W, the sum of every card's own hardware maximum - a budget above that can never actually bind on anything"
            )));
        }
        self.budget = Some(w);
        self.save_config();
        Ok(())
    }

    fn set_power(&mut self, i: usize, watts: &Value, confirm: bool) -> Result<(), OpErr> {
        let ranges: BTreeMap<String, Option<(i64, i64)>> =
            self.cards.iter().map(|c| (c.info.uuid.clone(), c.range)).collect();
        let current: BTreeMap<String, i64> =
            self.cards.iter().map(|c| (c.info.uuid.clone(), c.settings.power_w.unwrap_or(0))).collect();
        let w = safety::check_power(&self.cards[i].info.uuid, watts, &ranges, &current, self.budget, confirm)?;
        self.apply_power(i, w).map_err(driver)?;
        self.cards[i].settings.power_w = Some(w);
        Ok(())
    }

    fn set_fan(&mut self, i: usize, req: &Value) -> Result<(), OpErr> {
        let g = self.cards[i].info.clone();
        if g.nfans == 0 {
            return Err(SafetyErr::Msg("this card has no controllable fans".into()).into());
        }
        let mode = match req.get("mode").and_then(|m| m.as_str()) {
            Some(m @ ("curve" | "manual" | "auto")) => m.to_string(),
            _ => return Err(SafetyErr::Msg("fan mode must be curve, manual or auto".into()).into()),
        };
        let mut fan = self.cards[i].settings.fan.clone();
        if let Some(c) = req.get("curve") {
            fan.curve = safety::validate_curve(c, full_by(&g), g.fan_min)?;
        }
        if let Some(p) = req.get("manual_pct") {
            fan.manual_pct = safety::validate_manual(p, g.fan_min)?;
        }
        fan.mode = mode;
        self.cards[i].settings.fan = fan;
        self.cards[i].rt = Rt::fresh();
        Ok(())
    }

    fn set_clock_cap(&mut self, i: usize, mhz: &Value) -> Result<(), OpErr> {
        let cap = safety::validate_clock_cap(mhz, &self.cards[i].info.clocks)?;
        match cap {
            None => self.hw.reset_clock_cap(i).map_err(driver)?,
            Some(c) => self.hw.set_clock_cap(i, c).map_err(driver)?,
        }
        self.cards[i].settings.clock_cap_mhz = cap;
        Ok(())
    }

    fn baseline(&mut self, i: usize, done: &mut Vec<&'static str>) -> Result<(), OpErr> {
        // Always-safe steps first, power last: power is the one the budget can refuse.
        if self.cards[i].info.nfans > 0 {
            let curve: Vec<Value> = default_curve_for(&self.cards[i].info).iter().map(|p| json!([p[0], p[1]])).collect();
            self.set_fan(i, &json!({"mode": "curve", "curve": curve}))?;
            done.push("fan curve");
        }
        if self.cards[i].settings.clock_cap_mhz.is_some() {
            self.set_clock_cap(i, &Value::Null)?;
            done.push("clock cap");
        }
        if let Some(base) = self.baseline_power(i) {
            self.set_power(i, &json!(base), false)?;
        }
        Ok(())
    }

    pub fn handle(&mut self, req: &Value) -> Value {
        let Some(obj) = req.as_object() else {
            return json!({"ok": false, "error": "request must be a JSON object"});
        };
        let op = obj.get("op").and_then(|o| o.as_str()).unwrap_or("");
        match op {
            "status" => return self.status(),
            "state" => return self.ui_state(),
            "set_budget" => {
                return match self.set_budget(obj.get("watts").unwrap_or(&Value::Null)) {
                    Ok(()) => self.status(),
                    Err(e) => json!({"ok": false, "error": e.message()}),
                }
            }
            "set_power" | "set_fan" | "set_clock_cap" | "baseline" => {}
            other => return json!({"ok": false, "error": format!("unknown op '{other}'")}),
        }
        let uuid = obj.get("uuid").and_then(|u| u.as_str()).unwrap_or("");
        let Some(i) = self.cards.iter().position(|c| c.info.uuid == uuid) else {
            return json!({"ok": false, "error": "no such GPU"});
        };
        let mut done: Vec<&'static str> = vec![];
        let res = match op {
            "set_power" => {
                let confirm = obj.get("confirm_override") == Some(&Value::Bool(true));
                self.set_power(i, obj.get("watts").unwrap_or(&Value::Null), confirm)
            }
            "set_fan" => self.set_fan(i, req),
            "set_clock_cap" => self.set_clock_cap(i, obj.get("mhz").unwrap_or(&Value::Null)),
            _ => self.baseline(i, &mut done),
        };
        // Each step mutates settings only after it succeeds, so saving here is right on both paths.
        if let Err(e) = self.save_state() {
            log(&format!("could not save {}: {e}", self.state_path.display()));
        }
        let now = self.now();
        self.tick(now);
        match res {
            Ok(()) => self.status(),
            Err(e) => {
                let (mut msg, extra) = match e {
                    OpErr::Safety(SafetyErr::Budget { msg, total_w, budget_w, others_w }) => {
                        (msg, Some((total_w, budget_w, others_w)))
                    }
                    OpErr::Safety(SafetyErr::Msg(m)) => (m, None),
                    OpErr::Driver(m) => (format!("the driver refused: {m}"), None),
                };
                if !done.is_empty() {
                    msg = format!("{} reset, but the power limit was not: {msg}", done.join(" and "));
                }
                let mut out = json!({"ok": false, "error": msg});
                if let Some((t, b, o)) = extra {
                    out["over_budget"] = json!(true);
                    out["total_w"] = json!(t);
                    out["budget_w"] = json!(b);
                    out["others_w"] = json!(o);
                }
                out
            }
        }
    }
}

/// Hand every fan back to the driver's own curve (also the `restore-fans` command).
pub fn restore_all(hw: &dyn Hw) {
    for (i, g) in hw.gpus().iter().enumerate() {
        if g.nfans > 0 {
            if let Err(e) = hw.set_fan_auto(i) {
                log(&format!("FAULT {}: could not hand fans back to the driver: {e}", g.name));
            }
        }
    }
    log("fans handed back to the driver");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gputuner-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn make(budget: Option<i64>, state: Option<Value>) -> (Daemon, Arc<FakeHw>, PathBuf) {
        make_with(Arc::new(FakeHw::pair()), budget, state)
    }

    fn make_with(fake: Arc<FakeHw>, budget: Option<i64>, state: Option<Value>) -> (Daemon, Arc<FakeHw>, PathBuf) {
        let dir = tmpdir();
        let path = dir.join("state.json");
        if let Some(s) = state {
            fs::write(&path, s.to_string()).unwrap();
        }
        let cfg = Config { gpu_budget_w: budget, ..Config::default() };
        let hw: Arc<dyn Hw> = fake.clone();
        let mut d = Daemon::new(hw, cfg, path, None);
        d.load_state();
        d.apply_startup();
        (d, fake, dir)
    }

    fn ok(r: &Value) -> bool {
        r["ok"] == json!(true)
    }

    fn set_curve_mode(d: &mut Daemon) {
        for c in d.cards.iter_mut() {
            c.settings.fan.mode = "curve".into();
        }
    }

    #[test]
    fn first_run_adopts_live_limits() {
        let (d, _, _) = make(Some(750), None);
        assert_eq!(d.cards[0].settings.power_w, Some(300));
        assert_eq!(d.cards[1].settings.power_w, Some(450));
        assert_eq!(d.cards[1].settings.fan.mode, "auto");
    }

    #[test]
    fn saved_state_over_budget_falls_back() {
        let state = json!({"version": 1, "gpus": {MAXQ: {"power_w": 325}, WS: {"power_w": 600}}});
        let (d, _, _) = make(Some(750), Some(state));
        assert!(d.cards[0].settings.power_w.unwrap() + d.cards[1].settings.power_w.unwrap() <= 750);
        assert!(d.warnings.iter().any(|w| w.contains("over the 750 W budget")));
    }

    #[test]
    fn saved_bad_curve_falls_back_to_the_default_curve() {
        let state = json!({"version": 1, "gpus": {WS: {"fan": {"mode": "curve", "curve": [[30, 10], [80, 100]]}}}});
        let (d, _, _) = make(Some(750), Some(state));
        let lab: Vec<[i64; 2]> = safety::LAB_CURVE.iter().map(|&(t, p)| [t, p]).collect();
        assert_eq!(d.cards[1].settings.fan.curve, lab);
        assert!(d.warnings.iter().any(|w| w.contains("saved fan settings rejected")));
    }

    #[test]
    fn set_power_writes_reads_back_and_persists() {
        let (mut d, fake, _) = make(Some(750), None);
        let r = d.handle(&json!({"op": "set_power", "uuid": WS, "watts": 425}));
        assert!(ok(&r), "{r}");
        assert_eq!(fake.with(|s| s.cards[1].limit_w), Some(425));
        let saved: Value = serde_json::from_str(&fs::read_to_string(&d.state_path).unwrap()).unwrap();
        assert_eq!(saved["gpus"][WS]["power_w"], json!(425));
        assert_eq!(saved["version"], json!(1));
    }

    #[test]
    fn budget_is_enforced_across_cards() {
        let (mut d, _, _) = make(Some(750), None);
        assert!(!ok(&d.handle(&json!({"op": "set_power", "uuid": MAXQ, "watts": 325}))));
        assert!(ok(&d.handle(&json!({"op": "set_power", "uuid": WS, "watts": 425}))));
        assert!(ok(&d.handle(&json!({"op": "set_power", "uuid": MAXQ, "watts": 325}))));
        assert!(!ok(&d.handle(&json!({"op": "set_power", "uuid": WS, "watts": 430}))));
    }

    #[test]
    fn over_budget_response_carries_the_confirm_fields_and_writes_nothing() {
        let (mut d, fake, _) = make(Some(750), None);
        let r = d.handle(&json!({"op": "set_power", "uuid": MAXQ, "watts": 325}));
        assert!(!ok(&r));
        assert_eq!(r["over_budget"], json!(true));
        assert_eq!((r["total_w"].clone(), r["budget_w"].clone()), (json!(775), json!(750)));
        assert_eq!(fake.with(|s| s.cards[0].limit_w), Some(300));
    }

    #[test]
    fn confirm_override_applies_despite_the_budget() {
        let (mut d, fake, _) = make(Some(750), None);
        let r = d.handle(&json!({"op": "set_power", "uuid": MAXQ, "watts": 325, "confirm_override": true}));
        assert!(ok(&r), "{r}");
        assert!(r.get("over_budget").is_none());
        assert_eq!(fake.with(|s| s.cards[0].limit_w), Some(325));
        assert!(r["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("over the 750 W budget")));
    }

    #[test]
    fn confirm_override_needs_a_real_true() {
        let (mut d, _, _) = make(Some(750), None);
        for sneaky in [json!("false"), json!("0"), json!(1), json!([]), json!({})] {
            let r = d.handle(&json!({"op": "set_power", "uuid": MAXQ, "watts": 325, "confirm_override": sneaky}));
            assert!(!ok(&r), "{sneaky}");
            assert_eq!(r["over_budget"], json!(true));
        }
    }

    #[test]
    fn set_budget_updates_persists_and_gates_later_raises() {
        let (mut d, _, dir) = make(Some(750), None);
        d.config_path = Some(dir.join("config.json"));
        let r = d.handle(&json!({"op": "set_budget", "watts": 800}));
        assert!(ok(&r), "{r}");
        assert_eq!(d.budget, Some(800));
        assert_eq!(r["budget_w"], json!(800));
        let cfg: Value = serde_json::from_str(&fs::read_to_string(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(cfg["gpu_budget_w"], json!(800));
        assert!(ok(&d.handle(&json!({"op": "set_power", "uuid": WS, "watts": 500}))));
        assert!(!ok(&d.handle(&json!({"op": "set_power", "uuid": WS, "watts": 501}))));
    }

    #[test]
    fn set_budget_rejects_nonsense_and_values_above_the_hardware_maximum() {
        let (mut d, _, _) = make(Some(750), None);
        for bad in [json!(0), json!(-100), json!("800"), json!(true), json!(1.5)] {
            assert!(!ok(&d.handle(&json!({"op": "set_budget", "watts": bad}))), "{bad}");
        }
        assert_eq!(d.budget, Some(750));
        assert!(ok(&d.handle(&json!({"op": "set_budget", "watts": 925}))));
        let r = d.handle(&json!({"op": "set_budget", "watts": 926}));
        assert!(!ok(&r));
        assert!(r["error"].as_str().unwrap().contains("925 W"));
        assert_eq!(d.budget, Some(925));
    }

    #[test]
    fn fan_loop_applies_the_curve_with_the_floor() {
        let (mut d, fake, _) = make(Some(750), None);
        set_curve_mode(&mut d);
        fake.with(|s| s.cards[0].temp = 65);
        d.tick(100.0);
        assert!(fake.with(|s| s.fan_writes.contains(&(0, 0, 75))), "lab curve: 65 C -> 75%");
        fake.with(|s| s.cards[1].temp = 90);
        d.tick(102.0);
        assert!(fake.with(|s| s.fan_writes.contains(&(1, 0, 100))));
        assert!(fake.with(|s| s.fan_writes.contains(&(1, 1, 100))), "both fans on the twin-fan card");
    }

    #[test]
    fn a_refused_fan_write_hands_fans_back_to_the_driver() {
        let (mut d, fake, _) = make(Some(750), None);
        set_curve_mode(&mut d);
        fake.with(|s| s.refuse_fan = true);
        d.tick(100.0);
        assert!(fake.with(|s| s.fan_auto.contains(&(0, 0))));
        assert!(d.cards[0].rt.fault.is_some());
        let st = d.status();
        assert!(st["gpus"][0]["fan"]["fault"].as_str().unwrap().contains("fan write refused"));
    }

    #[test]
    fn an_unreadable_temperature_hands_fans_back_to_the_driver() {
        let (mut d, fake, _) = make(Some(750), None);
        set_curve_mode(&mut d);
        fake.with(|s| s.refuse_temp = true);
        d.tick(100.0);
        assert!(fake.with(|s| !s.fan_auto.is_empty()));
        assert!(d.cards[1].rt.fault.as_deref().unwrap().contains("temperature unreadable"));
    }

    #[test]
    fn a_faulted_card_is_not_retried_until_the_retry_time() {
        let (mut d, fake, _) = make(Some(750), None);
        set_curve_mode(&mut d);
        fake.with(|s| s.refuse_fan = true);
        d.tick(100.0);
        fake.with(|s| {
            s.refuse_fan = false;
            s.fan_writes.clear();
        });
        d.tick(110.0);
        assert!(fake.with(|s| s.fan_writes.is_empty()), "still inside the 30 s retry window");
        d.tick(131.0);
        assert!(fake.with(|s| !s.fan_writes.is_empty()), "retried after the window");
    }

    #[test]
    fn auto_mode_writes_the_driver_default_once() {
        let (mut d, fake, _) = make(Some(750), None);
        assert!(ok(&d.handle(&json!({"op": "set_fan", "uuid": WS, "mode": "auto"}))));
        let n = fake.with(|s| s.fan_auto.len());
        d.tick(200.0);
        d.tick(202.0);
        assert_eq!(fake.with(|s| s.fan_auto.len()), n);
        assert!(fake.with(|s| !s.fan_writes.iter().any(|w| w.0 == 1)));
    }

    #[test]
    fn a_drifted_power_limit_is_reapplied() {
        let (mut d, fake, _) = make(Some(750), None);
        fake.with(|s| s.cards[1].limit_w = Some(600));
        d.tick(1000.0);
        assert_eq!(fake.with(|s| s.cards[1].limit_w), Some(450));
    }

    #[test]
    fn baseline_resets_everything() {
        let (mut d, fake, _) = make(Some(750), None);
        d.handle(&json!({"op": "set_power", "uuid": WS, "watts": 400}));
        d.handle(&json!({"op": "set_fan", "uuid": WS, "mode": "manual", "manual_pct": 70}));
        d.handle(&json!({"op": "set_clock_cap", "uuid": WS, "mhz": 2000}));
        let r = d.handle(&json!({"op": "baseline", "uuid": WS}));
        assert!(ok(&r), "{r}");
        let s = &d.cards[1].settings;
        assert_eq!((s.power_w, s.fan.mode.as_str(), s.clock_cap_mhz), (Some(450), "curve", None));
        assert_eq!(fake.with(|s| s.clock_calls.last().cloned()), Some((1, None)));
    }

    #[test]
    fn clock_cap_snaps_down_and_clears() {
        let (mut d, fake, _) = make(Some(750), None);
        let r = d.handle(&json!({"op": "set_clock_cap", "uuid": MAXQ, "mhz": 2001}));
        assert_eq!(r["gpus"][0]["settings"]["clock_cap_mhz"], json!(1995));
        assert_eq!(fake.with(|s| s.clock_calls.last().cloned()), Some((0, Some((180, 1995)))));
        assert!(!ok(&d.handle(&json!({"op": "set_clock_cap", "uuid": MAXQ, "mhz": 500}))));
    }

    #[test]
    fn unknown_ops_gpus_and_non_objects_are_rejected() {
        let (mut d, _, _) = make(Some(750), None);
        assert!(!ok(&d.handle(&json!({"op": "reboot"}))));
        assert!(!ok(&d.handle(&json!({"op": "set_power", "uuid": "GPU-nope", "watts": 300}))));
        assert!(!ok(&d.handle(&json!(["not", "a", "dict"]))));
        assert!(!ok(&d.handle(&json!({"op": "set_fan", "uuid": WS, "mode": "turbo"}))));
    }

    #[test]
    fn restore_fans_covers_every_fan() {
        let (d, fake, _) = make(Some(750), None);
        let before = fake.with(|s| s.fan_auto.len());
        d.restore_fans();
        let mut added: Vec<(usize, u32)> = fake.with(|s| s.fan_auto[before..].to_vec());
        added.sort();
        assert_eq!(added, vec![(0, 0), (1, 0), (1, 1)]);
    }

    #[test]
    fn status_matches_the_shape_the_page_reads() {
        let (d, _, _) = make(Some(750), None);
        let st = d.status();
        for k in ["ok", "version", "budget_w", "budget_used_w", "power_settable", "curve_presets", "warnings", "gpus"] {
            assert!(st.get(k).is_some(), "{k}");
        }
        let g = &st["gpus"][1];
        assert_eq!(g["power_range"], json!([150, 600]));
        assert_eq!(g["clock_range"], json!([1000, 3090]));
        assert_eq!(g["profile"]["label"], json!("Workstation"));
        assert_eq!(g["full_by_c"], json!(85));
        assert!(st["curve_presets"]["quiet"]["curve"].is_array());
        let ui = d.ui_state();
        assert_eq!(ui["static"][0]["uuid"], json!(MAXQ));
        assert!(ui["live"][MAXQ]["temp"].is_number());
    }

    #[test]
    fn gb10_has_no_power_range_fans_or_clocks_and_nothing_is_written() {
        let (mut d, fake, _) = make_with(Arc::new(FakeHw::gb10()), Some(750), None);
        assert_eq!(d.cards[0].range, None);
        assert_eq!(d.cards[0].settings.power_w, None);
        assert!(fake.with(|s| s.power_writes.is_empty()), "no write was attempted");
        let r = d.handle(&json!({"op": "set_power", "uuid": GB10, "watts": 100}));
        assert!(!ok(&r));
        assert!(r["error"].as_str().unwrap().contains("no settable power range"));
        let r = d.handle(&json!({"op": "set_fan", "uuid": GB10, "mode": "manual", "manual_pct": 50}));
        assert!(r["error"].as_str().unwrap().contains("no controllable fans"));
        let r = d.handle(&json!({"op": "set_budget", "watts": 100}));
        assert!(r["error"].as_str().unwrap().contains("nothing for a budget to limit"));
        assert_eq!(d.status()["gpus"][0]["clock_range"], Value::Null);
        assert!(!d.warnings.iter().any(|w| w.contains("persistence")), "unsupported persistence is not a warning");
    }

    #[test]
    fn a_power_write_the_driver_silently_ignores_is_reported() {
        let fake = Arc::new(FakeHw::pair());
        let (mut d, _, _) = make_with(fake.clone(), Some(750), None);
        fake.with(|s| s.cards[1].power_supported = false);
        let r = d.handle(&json!({"op": "set_power", "uuid": WS, "watts": 425}));
        assert!(!ok(&r));
        assert!(r["error"].as_str().unwrap().starts_with("the driver refused:"));
        assert_eq!(d.cards[1].settings.power_w, Some(450), "settings only change after the write succeeds");
    }

    #[test]
    fn config_loading_follows_the_upstream_rules() {
        let dir = tmpdir();
        let p = dir.join("config.json");
        assert_eq!(load_config(&p).unwrap().gpu_budget_w, Some(750), "missing file: default budget");
        fs::write(&p, r#"{"allowed_uid": null, "gpu_budget_w": null, "interval_s": 2.0}"#).unwrap();
        assert_eq!(load_config(&p).unwrap().gpu_budget_w, None, "explicit null: no budget");
        fs::write(&p, r#"{"gpu_budget_w": 600, "interval_s": 5}"#).unwrap();
        let c = load_config(&p).unwrap();
        assert_eq!((c.gpu_budget_w, c.interval_s), (Some(600), 5.0));
        for bad in [r#"{"gpu_budget_w": 0}"#, r#"{"gpu_budget_w": "x"}"#, r#"{"interval_s": 0.5}"#, r#"{"interval_s": 11}"#, r#"{"allowed_uid": -1}"#, "[]", "nope"] {
            fs::write(&p, bad).unwrap();
            assert!(load_config(&p).is_err(), "{bad}");
        }
    }

    #[test]
    fn state_and_budget_survive_a_restart() {
        let (mut d, _, dir) = make(Some(750), None);
        d.config_path = Some(dir.join("config.json"));
        d.handle(&json!({"op": "set_power", "uuid": WS, "watts": 400}));
        d.handle(&json!({"op": "set_fan", "uuid": WS, "mode": "manual", "manual_pct": 66}));
        d.handle(&json!({"op": "set_budget", "watts": 800}));
        let cfg = load_config(&dir.join("config.json")).unwrap();
        assert_eq!(cfg.gpu_budget_w, Some(800));
        let hw: Arc<dyn Hw> = Arc::new(FakeHw::pair());
        let mut d2 = Daemon::new(hw, cfg, dir.join("state.json"), None);
        d2.load_state();
        assert_eq!(d2.cards[1].settings.power_w, Some(400));
        assert_eq!(d2.cards[1].settings.fan.manual_pct, 66);
        assert_eq!(d2.cards[1].settings.fan.mode, "manual");
    }
}
