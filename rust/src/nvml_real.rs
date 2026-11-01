//! The real driver: NVML through the `nvml-wrapper` crate (which loads libnvidia-ml.so.1 at run
//! time, so the binary starts and reports a clean error on a machine without the NVIDIA driver).

use crate::hw::*;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor, TemperatureThreshold};
use nvml_wrapper::enums::device::GpuLockedClocksSetting;
use nvml_wrapper::error::NvmlError;
use nvml_wrapper::Nvml;
use std::collections::BTreeMap;

pub struct RealHw {
    nvml: Nvml,
    infos: Vec<GpuInfo>,
    driver: String,
}

fn code_of(e: &NvmlError) -> Option<i32> {
    match e {
        NvmlError::NotSupported => Some(NOT_SUPPORTED),
        NvmlError::FunctionNotFound | NvmlError::FailedToLoadSymbol(_) => Some(FUNCTION_NOT_FOUND),
        _ => None,
    }
}

fn herr(e: NvmlError) -> HwError {
    HwError { code: code_of(&e), msg: e.to_string() }
}

/// Python-style rounding (half to even) of milliwatts to whole watts, as upstream did.
fn mw_to_w(mw: u32) -> i64 {
    (mw as f64 / 1000.0).round_ties_even() as i64
}

fn system_memory_mib() -> (Option<i64>, Option<i64>) {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else { return (None, None) };
    let field = |name: &str| -> Option<i64> {
        text.lines().find(|l| l.starts_with(name))?.split_whitespace().nth(1)?.parse().ok()
    };
    match (field("MemTotal:"), field("MemAvailable:")) {
        (Some(total), Some(avail)) => (Some((total - avail) >> 10), Some(total >> 10)),
        _ => (None, None),
    }
}

impl RealHw {
    pub fn new() -> Result<RealHw, String> {
        let nvml = Nvml::init().map_err(|e| format!("NVML unavailable: {e}"))?;
        let driver = nvml.sys_driver_version().unwrap_or_default();
        let n = nvml.device_count().map_err(|e| e.to_string())?;
        let mut infos = Vec::new();
        for i in 0..n {
            infos.push(Self::load(&nvml, i)?);
        }
        Ok(RealHw { nvml, infos, driver })
    }

    fn load(nvml: &Nvml, i: u32) -> Result<GpuInfo, String> {
        let d = nvml.device_by_index(i).map_err(|e| e.to_string())?;
        let uuid = d.uuid().map_err(|e| e.to_string())?;
        let name = d.name().map_err(|e| e.to_string())?;
        let (fan_min, fan_max) = d.min_max_fan_speed().map(|(a, b)| (a as i64, b as i64)).unwrap_or((30, 100));
        let (mut power_min_w, mut power_max_w) = (None, None);
        if let Ok(c) = d.power_management_limit_constraints() {
            if c.max_limit > 0 {
                power_min_w = Some(c.min_limit as i64 / 1000);
                power_max_w = Some(c.max_limit as i64 / 1000);
            }
        }
        let power_default_w = d.power_management_limit_default().ok().map(|v| v as i64 / 1000).filter(|&w| w > 0);
        let mut thresholds = BTreeMap::new();
        for (key, t) in [
            ("t_limit", TemperatureThreshold::GpuMax),
            ("slowdown", TemperatureThreshold::Slowdown),
            ("shutdown", TemperatureThreshold::Shutdown),
        ] {
            if let Ok(v) = d.temperature_threshold(t) {
                if v > 0 {
                    thresholds.insert(key.to_string(), v as i64);
                }
            }
        }
        let mut clocks: Vec<i64> = vec![];
        if let Some(top_mem) = d.supported_memory_clocks().ok().and_then(|m| m.into_iter().max()) {
            clocks = d.supported_graphics_clocks(top_mem).unwrap_or_default().into_iter().map(|c| c as i64).collect();
            clocks.sort_unstable();
        }
        let mem_kind = match d.memory_info() {
            Err(NvmlError::NotSupported) => "unified",
            _ => "dedicated",
        };
        Ok(GpuInfo {
            index: i as usize,
            uuid,
            name,
            pci: d.pci_info().map(|p| p.bus_id).unwrap_or_default(),
            vbios: d.vbios_version().unwrap_or_default(),
            nfans: d.num_fans().unwrap_or(0),
            fan_min,
            fan_max,
            power_min_w,
            power_max_w,
            power_default_w,
            thresholds,
            mem_kind: mem_kind.to_string(),
            clocks,
            vendor: "nvidia",
            display_clock_max: None,
        })
    }

    fn dev(&self, i: usize) -> HResult<nvml_wrapper::Device<'_>> {
        self.nvml.device_by_index(i as u32).map_err(herr)
    }
}

impl Hw for RealHw {
    fn driver(&self) -> String {
        self.driver.clone()
    }

    fn gpus(&self) -> &[GpuInfo] {
        &self.infos
    }

    fn temp_c(&self, i: usize) -> HResult<i64> {
        self.dev(i)?.temperature(TemperatureSensor::Gpu).map(|t| t as i64).map_err(herr)
    }

    fn power_limit_w(&self, i: usize) -> Option<i64> {
        self.dev(i).ok()?.power_management_limit().ok().map(mw_to_w)
    }

    fn sample(&self, i: usize) -> Sample {
        let info = &self.infos[i];
        let Ok(d) = self.dev(i) else { return Sample { mem_kind: info.mem_kind.clone(), ..Default::default() } };
        let util = d.utilization_rates().ok();
        let (used, total) = if info.mem_kind == "unified" {
            system_memory_mib()
        } else {
            match d.memory_info() {
                Ok(m) => (Some((m.used >> 20) as i64), Some((m.total >> 20) as i64)),
                Err(_) => (None, None),
            }
        };
        Sample {
            temp: d.temperature(TemperatureSensor::Gpu).ok().map(|t| t as i64),
            fans: (0..info.nfans).map(|f| d.fan_speed(f).ok().map(|s| s as i64)).collect(),
            power_w: d.power_usage().ok().map(|mw| (mw as f64 / 100.0).round() / 10.0),
            power_limit_w: d.power_management_limit().ok().map(mw_to_w),
            util: util.as_ref().map(|u| u.gpu as i64),
            mem_util: util.as_ref().map(|u| u.memory as i64),
            vram_used_mib: used,
            vram_total_mib: total,
            mem_kind: info.mem_kind.clone(),
            clock_mhz: d.clock_info(Clock::Graphics).ok().map(|c| c as i64),
            mem_clock_mhz: d.clock_info(Clock::Memory).ok().map(|c| c as i64),
            pstate: d.performance_state().ok().map(|p| p.as_c() as i64),
            reasons: d.current_throttle_reasons().map(|r| reasons_from_mask(r.bits())).unwrap_or_default(),
        }
    }

    #[cfg(target_os = "linux")]
    fn set_persistence(&self, i: usize) -> HResult<()> {
        self.dev(i)?.set_persistent(true).map_err(herr)
    }

    #[cfg(not(target_os = "linux"))]
    fn set_persistence(&self, _i: usize) -> HResult<()> {
        Err(HwError::with_code(NOT_SUPPORTED, "persistence mode is only available on Linux"))
    }

    fn set_power_limit_w(&self, i: usize, watts: i64) -> HResult<()> {
        self.dev(i)?.set_power_management_limit((watts * 1000) as u32).map_err(herr)
    }

    fn set_fan_pct(&self, i: usize, pct: i64) -> HResult<()> {
        let mut d = self.dev(i)?;
        for f in 0..self.infos[i].nfans {
            d.set_fan_speed(f, pct as u32).map_err(herr)?;
        }
        Ok(())
    }

    fn set_fan_auto(&self, i: usize) -> HResult<()> {
        let mut d = self.dev(i)?;
        let mut last = None;
        for f in 0..self.infos[i].nfans {
            if let Err(e) = d.set_default_fan_speed(f) {
                last = Some(herr(e));
            }
        }
        match last {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn set_clock_cap(&self, i: usize, mhz: i64) -> HResult<()> {
        let lo = self.infos[i].clocks.iter().copied().min().unwrap_or(0);
        self.dev(i)?
            .set_gpu_locked_clocks(GpuLockedClocksSetting::Numeric { min_clock_mhz: lo as u32, max_clock_mhz: mhz as u32 })
            .map_err(herr)
    }

    fn reset_clock_cap(&self, i: usize) -> HResult<()> {
        self.dev(i)?.reset_gpu_locked_clocks().map_err(herr)
    }
}
