//! The hardware boundary. The daemon only ever talks to a `Hw`, so the same logic runs against
//! real NVML (`nvml_real`) and against a fake in tests and in `--fake` mode.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// NVML return codes the daemon cares about.
pub const NOT_SUPPORTED: i32 = 3;
pub const FUNCTION_NOT_FOUND: i32 = 13;

#[derive(Debug, Clone)]
pub struct HwError {
    pub code: Option<i32>,
    pub msg: String,
}

impl HwError {
    pub fn new(msg: impl Into<String>) -> Self {
        HwError { code: None, msg: msg.into() }
    }
    pub fn with_code(code: i32, msg: impl Into<String>) -> Self {
        HwError { code: Some(code), msg: msg.into() }
    }
    /// A fact about the card (it simply lacks the feature), not a fault to keep showing.
    pub fn not_supported(&self) -> bool {
        matches!(self.code, Some(NOT_SUPPORTED) | Some(FUNCTION_NOT_FOUND))
    }
}

impl std::fmt::Display for HwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

pub type HResult<T> = Result<T, HwError>;

#[derive(Debug, Clone)]
pub struct GpuInfo {
    pub index: usize,
    pub uuid: String,
    pub name: String,
    pub pci: String,
    pub vbios: String,
    pub nfans: u32,
    pub fan_min: i64,
    pub fan_max: i64,
    pub power_min_w: Option<i64>,
    pub power_max_w: Option<i64>,
    pub power_default_w: Option<i64>,
    pub thresholds: BTreeMap<String, i64>,
    pub mem_kind: String,
    /// Supported graphics clocks (MHz) at the top memory clock, sorted.
    pub clocks: Vec<i64>,
}

impl GpuInfo {
    pub fn slowdown(&self) -> Option<i64> {
        self.thresholds.get("slowdown").copied()
    }

    pub fn static_json(&self) -> Value {
        json!({
            "index": self.index, "uuid": self.uuid, "name": self.name, "pci": self.pci,
            "vbios": self.vbios, "nfans": self.nfans, "fan_min": self.fan_min, "fan_max": self.fan_max,
            "power_min_w": self.power_min_w, "power_max_w": self.power_max_w,
            "power_default_w": self.power_default_w, "thresholds": self.thresholds,
            "mem_kind": self.mem_kind,
            "clock_min_mhz": self.clocks.iter().min(), "clock_max_mhz": self.clocks.iter().max(),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Sample {
    pub temp: Option<i64>,
    pub fans: Vec<Option<i64>>,
    pub power_w: Option<f64>,
    pub power_limit_w: Option<i64>,
    pub util: Option<i64>,
    pub mem_util: Option<i64>,
    pub vram_used_mib: Option<i64>,
    pub vram_total_mib: Option<i64>,
    pub mem_kind: String,
    pub clock_mhz: Option<i64>,
    pub mem_clock_mhz: Option<i64>,
    pub pstate: Option<i64>,
    pub reasons: Vec<String>,
}

pub trait Hw: Send + Sync {
    fn driver(&self) -> String;
    fn gpus(&self) -> &[GpuInfo];
    fn dry_run(&self) -> bool {
        false
    }
    /// Raises on failure: the fan loop must KNOW it has no reading, not get a default.
    fn temp_c(&self, i: usize) -> HResult<i64>;
    fn power_limit_w(&self, i: usize) -> Option<i64>;
    fn sample(&self, i: usize) -> Sample;
    fn set_persistence(&self, i: usize) -> HResult<()>;
    fn set_power_limit_w(&self, i: usize, watts: i64) -> HResult<()>;
    /// All fans of the card.
    fn set_fan_pct(&self, i: usize, pct: i64) -> HResult<()>;
    /// Hand every fan back to the driver's own curve. Tries ALL fans even if one fails.
    fn set_fan_auto(&self, i: usize) -> HResult<()>;
    fn set_clock_cap(&self, i: usize, mhz: i64) -> HResult<()>;
    fn reset_clock_cap(&self, i: usize) -> HResult<()>;
}

/// Bit values of NVML's clocks-event reasons, in the order they are reported.
pub const REASONS: [(u64, &str); 9] = [
    (0x0001, "idle"),
    (0x0002, "app_clocks"),
    (0x0004, "sw_power_cap"),
    (0x0008, "hw_slowdown"),
    (0x0010, "sync_boost"),
    (0x0020, "sw_thermal"),
    (0x0040, "hw_thermal"),
    (0x0080, "hw_power_brake"),
    (0x0100, "display_clock"),
];

pub fn reasons_from_mask(mask: u64) -> Vec<String> {
    REASONS.iter().filter(|(bit, _)| mask & bit != 0).map(|(_, k)| k.to_string()).collect()
}
