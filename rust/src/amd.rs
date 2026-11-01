//! AMD GPUs through the amdgpu driver's sysfs files (no vendor library needed).
//!
//! EXPERIMENTAL. This was written against the kernel's amdgpu documentation and tested against a
//! simulated /sys tree, not on real AMD hardware. Use at your own risk.
//!
//! What it does: live readings, a power cap (lower than the card's default only, because an unknown
//! card has no profile), and fan control through pwm1 with the same safety floor as NVIDIA cards.
//! What it does not do yet: clock caps (they differ by AMD generation), throttle reasons, and fan
//! curves on cards that only expose the newer fan_curve interface (RX 7000 / RDNA3).

use crate::hw::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const AMD_VENDOR: &str = "0x1002";
const PWM_DEFAULT_MAX: i64 = 255;

struct Card {
    dev: PathBuf,
    hwmon: PathBuf,
    has_pwm: bool,
    pwm_max: i64,
}

pub struct AmdHw {
    cards: Vec<Card>,
    infos: Vec<GpuInfo>,
}

fn read(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn read_i64(p: &Path) -> Option<i64> {
    read(p)?.parse().ok()
}

/// A missing file means the card simply lacks that feature; anything else is a real error.
fn io_err(p: &Path, e: std::io::Error) -> HwError {
    let file = p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    if e.kind() == std::io::ErrorKind::NotFound {
        HwError::with_code(NOT_SUPPORTED, format!("{file} is not available on this card"))
    } else {
        HwError::new(format!("{file}: {e}"))
    }
}

fn write(p: &Path, value: &str) -> HResult<()> {
    fs::write(p, value).map_err(|e| io_err(p, e))
}

fn uw_to_w(uw: i64) -> i64 {
    (uw as f64 / 1e6).round_ties_even() as i64
}

fn milli_to_c(m: i64) -> i64 {
    (m as f64 / 1000.0).round_ties_even() as i64
}

fn uevent(dev: &Path) -> BTreeMap<String, String> {
    read(&dev.join("uevent"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn hwmon_dir(dev: &Path) -> Option<PathBuf> {
    fs::read_dir(dev.join("hwmon"))
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("hwmon")).unwrap_or(false))
        .min()
}

/// The marketing name of an AMD device id from the system's pci.ids, e.g. 73bf ->
/// "AMD Radeon RX 6800/6800 XT / 6900 XT".
fn pci_name(pci_ids: &Path, device_id: &str) -> Option<String> {
    let text = fs::read_to_string(pci_ids).ok()?;
    let mut in_amd = false;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            in_amd = line.starts_with("1002 ");
            continue;
        }
        if in_amd && !line.starts_with("\t\t") {
            let (id, name) = line[1..].split_once(char::is_whitespace)?;
            if id.eq_ignore_ascii_case(device_id) {
                let name = name.trim();
                return Some(match (name.find('['), name.rfind(']')) {
                    (Some(a), Some(b)) if b > a => format!("AMD {}", &name[a + 1..b]),
                    _ => format!("AMD {name}"),
                });
            }
        }
    }
    None
}

/// (active level, highest level) in MHz from a pp_dpm_* file: "1: 1500Mhz *".
fn dpm_levels(p: &Path) -> (Option<i64>, Option<i64>) {
    let Some(text) = read(p) else { return (None, None) };
    let (mut active, mut max): (Option<i64>, Option<i64>) = (None, None);
    for line in text.lines() {
        let mhz = line
            .split(':')
            .nth(1)
            .and_then(|r| r.trim().split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|d| d.parse::<i64>().ok());
        if let Some(m) = mhz {
            max = Some(max.map_or(m, |x| x.max(m)));
            if line.trim_end().ends_with('*') {
                active = Some(m);
            }
        }
    }
    (active, max)
}

impl AmdHw {
    /// Find every amdgpu card under `sys` (normally /sys). None when there is none.
    pub fn discover(sys: &Path, pci_ids: &Path) -> Option<AmdHw> {
        let drm = sys.join("class/drm");
        let mut names: Vec<(u32, String)> = fs::read_dir(&drm)
            .ok()?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                let idx = n.strip_prefix("card")?.parse::<u32>().ok()?;
                Some((idx, n))
            })
            .collect();
        names.sort();
        let (mut cards, mut infos) = (Vec::new(), Vec::new());
        for (_, name) in names {
            let dev = drm.join(&name).join("device");
            if read(&dev.join("vendor")).map(|v| v.to_lowercase()).as_deref() != Some(AMD_VENDOR) {
                continue;
            }
            let ue = uevent(&dev);
            if ue.get("DRIVER").map(String::as_str) != Some("amdgpu") {
                continue;
            }
            let Some(hwmon) = hwmon_dir(&dev) else { continue };
            let slot = ue.get("PCI_SLOT_NAME").cloned().unwrap_or_else(|| name.clone());
            let dev_id = read(&dev.join("device")).unwrap_or_default().trim_start_matches("0x").to_lowercase();
            let display_name = read(&dev.join("product_name"))
                .filter(|s| !s.is_empty())
                .or_else(|| pci_name(pci_ids, &dev_id))
                .unwrap_or_else(|| format!("AMD GPU (1002:{dev_id})"));

            let cap_default = read_i64(&hwmon.join("power1_cap_default"))
                .or_else(|| read_i64(&hwmon.join("power1_cap")))
                .map(|c| c / 1_000_000)
                .filter(|&w| w > 0);
            let (power_min_w, power_max_w) = match (read_i64(&hwmon.join("power1_cap_min")), read_i64(&hwmon.join("power1_cap_max"))) {
                (Some(lo), Some(hi)) if hi > 0 => {
                    let lo_w = lo / 1_000_000;
                    // A card that reports no real minimum must not be allowed down to 0 W.
                    let floor = match cap_default {
                        Some(d) if lo_w < 10 => d / 2,
                        _ => lo_w,
                    };
                    (Some(floor), Some(hi / 1_000_000))
                }
                _ => (None, None),
            };
            let mut thresholds = BTreeMap::new();
            if let Some(c) = read_i64(&hwmon.join("temp1_crit")).filter(|&c| c > 0) {
                thresholds.insert("slowdown".to_string(), c / 1000);
            }
            if let Some(c) = read_i64(&hwmon.join("temp1_emergency")).filter(|&c| c > 0) {
                thresholds.insert("shutdown".to_string(), c / 1000);
            }
            let has_pwm = hwmon.join("pwm1").exists() && hwmon.join("pwm1_enable").exists();
            let pwm_max = read_i64(&hwmon.join("pwm1_max")).filter(|&m| m > 0).unwrap_or(PWM_DEFAULT_MAX);

            infos.push(GpuInfo {
                index: cards.len(),
                uuid: format!("AMD-{slot}"),
                name: display_name,
                pci: slot,
                vbios: read(&dev.join("vbios_version")).unwrap_or_default(),
                nfans: u32::from(has_pwm),
                fan_min: 30,
                fan_max: 100,
                power_min_w,
                power_max_w,
                power_default_w: cap_default,
                thresholds,
                mem_kind: "dedicated".into(),
                clocks: vec![],
                vendor: "amd",
                display_clock_max: dpm_levels(&dev.join("pp_dpm_sclk")).1,
            });
            cards.push(Card { dev, hwmon, has_pwm, pwm_max });
        }
        if cards.is_empty() {
            None
        } else {
            Some(AmdHw { cards, infos })
        }
    }

    fn hw(&self, i: usize, file: &str) -> PathBuf {
        self.cards[i].hwmon.join(file)
    }
}

impl Hw for AmdHw {
    fn driver(&self) -> String {
        "amdgpu".into()
    }

    fn gpus(&self) -> &[GpuInfo] {
        &self.infos
    }

    fn temp_c(&self, i: usize) -> HResult<i64> {
        read_i64(&self.hw(i, "temp1_input")).map(milli_to_c).ok_or_else(|| HwError::new("temperature unreadable (temp1_input)"))
    }

    fn power_limit_w(&self, i: usize) -> Option<i64> {
        read_i64(&self.hw(i, "power1_cap")).map(uw_to_w)
    }

    fn sample(&self, i: usize) -> Sample {
        let (c, info) = (&self.cards[i], &self.infos[i]);
        let h = |f: &str| read_i64(&c.hwmon.join(f));
        let d = |f: &str| read_i64(&c.dev.join(f));
        let fans = if c.has_pwm {
            vec![h("pwm1").map(|v| (v as f64 * 100.0 / c.pwm_max as f64).round() as i64)]
        } else if let (Some(rpm), Some(max)) = (h("fan1_input"), h("fan1_max")) {
            if max > 0 { vec![Some(rpm * 100 / max)] } else { vec![] }
        } else {
            vec![]
        };
        Sample {
            temp: h("temp1_input").map(milli_to_c),
            fans,
            power_w: h("power1_average").or_else(|| h("power1_input")).map(|uw| (uw as f64 / 1e5).round() / 10.0),
            power_limit_w: h("power1_cap").map(uw_to_w),
            util: d("gpu_busy_percent"),
            mem_util: d("mem_busy_percent"),
            vram_used_mib: d("mem_info_vram_used").map(|b| b >> 20),
            vram_total_mib: d("mem_info_vram_total").map(|b| b >> 20),
            mem_kind: info.mem_kind.clone(),
            clock_mhz: h("freq1_input").map(|hz| hz / 1_000_000).or(dpm_levels(&c.dev.join("pp_dpm_sclk")).0),
            mem_clock_mhz: h("freq2_input").map(|hz| hz / 1_000_000).or(dpm_levels(&c.dev.join("pp_dpm_mclk")).0),
            pstate: None,
            reasons: vec![],
        }
    }

    fn set_persistence(&self, _i: usize) -> HResult<()> {
        Err(HwError::with_code(NOT_SUPPORTED, "persistence mode does not apply to AMD GPUs"))
    }

    fn set_power_limit_w(&self, i: usize, watts: i64) -> HResult<()> {
        write(&self.hw(i, "power1_cap"), &(watts * 1_000_000).to_string())
    }

    fn set_fan_pct(&self, i: usize, pct: i64) -> HResult<()> {
        let c = &self.cards[i];
        if !c.has_pwm {
            return Err(HwError::with_code(NOT_SUPPORTED, "this card has no controllable fan"));
        }
        write(&self.hw(i, "pwm1_enable"), "1")?;
        let value = (pct as f64 * c.pwm_max as f64 / 100.0).round() as i64;
        write(&self.hw(i, "pwm1"), &value.to_string())
    }

    fn set_fan_auto(&self, i: usize) -> HResult<()> {
        if !self.cards[i].has_pwm {
            return Ok(());
        }
        write(&self.hw(i, "pwm1_enable"), "2")
    }

    fn set_clock_cap(&self, _i: usize, _mhz: i64) -> HResult<()> {
        Err(HwError::with_code(NOT_SUPPORTED, "clock caps are not supported for AMD GPUs yet"))
    }

    fn reset_clock_cap(&self, _i: usize) -> HResult<()> {
        Ok(())
    }
}

/// A plain-text report of every AMD card's amdgpu sysfs files, for `gputuner probe`. Read-only.
/// It lists what exists even for features this plugin does not use yet, so a tester's output shows
/// exactly what a new card generation offers.
pub fn diagnose(sys: &Path) -> String {
    const HWMON_FILES: [&str; 17] = [
        "temp1_input", "temp1_crit", "temp2_input", "temp3_input", "power1_average", "power1_input", "power1_cap",
        "power1_cap_min", "power1_cap_max", "power1_cap_default", "pwm1", "pwm1_enable", "pwm1_max", "fan1_input",
        "fan1_max", "freq1_input", "freq2_input",
    ];
    const DEV_FILES: [&str; 6] = [
        "gpu_busy_percent", "mem_info_vram_total", "mem_info_vram_used", "power_dpm_force_performance_level",
        "pp_dpm_sclk", "pp_dpm_mclk",
    ];
    let drm = sys.join("class/drm");
    let mut names: Vec<(u32, String)> = fs::read_dir(&drm)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            Some((n.strip_prefix("card")?.parse::<u32>().ok()?, n))
        })
        .collect();
    names.sort();
    let mut out = String::new();
    for (_, name) in names {
        let dev = drm.join(&name).join("device");
        if read(&dev.join("vendor")).map(|v| v.to_lowercase()).as_deref() != Some(AMD_VENDOR) {
            continue;
        }
        let ue = uevent(&dev);
        out.push_str(&format!(
            "[{name}] AMD device {}  driver={}  slot={}\n",
            read(&dev.join("device")).unwrap_or_else(|| "?".into()),
            ue.get("DRIVER").map(String::as_str).unwrap_or("none"),
            ue.get("PCI_SLOT_NAME").map(String::as_str).unwrap_or("?"),
        ));
        for f in DEV_FILES {
            let v = read(&dev.join(f)).map(|s| s.lines().take(3).collect::<Vec<_>>().join(" | "));
            out.push_str(&format!("    {f}: {}\n", v.unwrap_or_else(|| "(missing)".into())));
        }
        let od = dev.join("gpu_od");
        let od_names: Vec<String> = fs::read_dir(&od)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        out.push_str(&format!("    gpu_od/: {}\n", if od_names.is_empty() { "(missing)".to_string() } else { od_names.join(", ") }));
        match hwmon_dir(&dev) {
            None => out.push_str("    hwmon: (none)\n"),
            Some(h) => {
                out.push_str(&format!("    hwmon: {}\n", h.file_name().unwrap_or_default().to_string_lossy()));
                for f in HWMON_FILES {
                    out.push_str(&format!("        {f}: {}\n", read(&h.join(f)).unwrap_or_else(|| "(missing)".into())));
                }
            }
        }
    }
    if out.is_empty() {
        out.push_str("no AMD GPU found under the given sysfs root\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{Config, Daemon};
    use serde_json::json;
    use std::sync::Arc;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gputuner-amd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn add_card(root: &Path, card: &str, vendor: &str, driver: &str, slot: &str, pwm: bool) {
        let d = format!("class/drm/{card}/device");
        put(root, &format!("{d}/vendor"), &format!("{vendor}\n"));
        put(root, &format!("{d}/device"), "0x73bf\n");
        put(root, &format!("{d}/uevent"), &format!("DRIVER={driver}\nPCI_ID=1002:73BF\nPCI_SLOT_NAME={slot}\n"));
        put(root, &format!("{d}/gpu_busy_percent"), "37\n");
        put(root, &format!("{d}/mem_info_vram_total"), "17163091968\n");
        put(root, &format!("{d}/mem_info_vram_used"), "2147483648\n");
        put(root, &format!("{d}/pp_dpm_sclk"), "0: 500Mhz\n1: 1500Mhz *\n2: 2400Mhz\n");
        let h = format!("{d}/hwmon/hwmon3");
        for (f, v) in [
            ("temp1_input", "62000"),
            ("temp1_crit", "100000"),
            ("power1_average", "151000000"),
            ("power1_cap", "203000000"),
            ("power1_cap_min", "100000000"),
            ("power1_cap_max", "250000000"),
            ("power1_cap_default", "203000000"),
        ] {
            put(root, &format!("{h}/{f}"), &format!("{v}\n"));
        }
        if pwm {
            for (f, v) in [("pwm1", "102"), ("pwm1_enable", "2"), ("pwm1_max", "255"), ("fan1_input", "1200")] {
                put(root, &format!("{h}/{f}"), &format!("{v}\n"));
            }
        }
    }

    const PCI_IDS: &str = "# list\n8086  Intel\n\t0001  Something\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t73bf  Navi 21 [Radeon RX 6800/6800 XT / 6900 XT]\n\t\t1002 0001  A subsystem\n\t73ff  Navi 23\n";

    fn one_card() -> (PathBuf, AmdHw) {
        let root = tmp();
        add_card(&root, "card0", "0x1002", "amdgpu", "0000:03:00.0", true);
        put(&root, "pci.ids", PCI_IDS);
        let hw = AmdHw::discover(&root, &root.join("pci.ids")).expect("one AMD card");
        (root, hw)
    }

    fn daemon(hw: AmdHw) -> Daemon {
        let state = tmp().join("state.json");
        let mut cfg = Config::default();
        cfg.gpu_budget_w = None;
        let mut d = Daemon::new(Arc::new(hw), cfg, state, None);
        d.load_state();
        d.apply_startup();
        d
    }

    #[test]
    fn it_finds_amdgpu_cards_and_ignores_everything_else() {
        let root = tmp();
        add_card(&root, "card0", "0x1002", "amdgpu", "0000:03:00.0", true);
        add_card(&root, "card1", "0x10de", "nvidia", "0000:04:00.0", true);
        add_card(&root, "card2", "0x1002", "radeon", "0000:05:00.0", true);
        put(&root, "class/drm/card0-DP-1/device/vendor", "0x1002\n");
        put(&root, "class/drm/renderD128/device/vendor", "0x1002\n");
        let hw = AmdHw::discover(&root, &root.join("none")).unwrap();
        assert_eq!(hw.gpus().len(), 1);
        assert_eq!(hw.gpus()[0].uuid, "AMD-0000:03:00.0");
        assert!(AmdHw::discover(&tmp(), &root.join("none")).is_none(), "no drm folder at all");
    }

    #[test]
    fn it_describes_the_card_the_way_the_page_expects() {
        let (_r, hw) = one_card();
        let g = &hw.gpus()[0];
        assert_eq!(g.name, "AMD Radeon RX 6800/6800 XT / 6900 XT");
        assert_eq!((g.vendor, g.index, g.nfans, g.fan_min), ("amd", 0, 1, 30));
        assert_eq!((g.power_min_w, g.power_max_w, g.power_default_w), (Some(100), Some(250), Some(203)));
        assert_eq!(g.slowdown(), Some(100));
        assert!(g.clocks.is_empty(), "no clock cap offered");
        assert_eq!(g.static_json()["clock_max_mhz"], json!(2400), "still shows the top clock");
        assert_eq!(g.static_json()["vendor"], json!("amd"));
    }

    #[test]
    fn it_falls_back_to_a_generic_name() {
        let (r, _) = one_card();
        assert_eq!(AmdHw::discover(&r, &r.join("missing")).unwrap().gpus()[0].name, "AMD GPU (1002:73bf)");
        put(&r, "class/drm/card0/device/product_name", "AMD Radeon PRO W6800\n");
        assert_eq!(AmdHw::discover(&r, &r.join("pci.ids")).unwrap().gpus()[0].name, "AMD Radeon PRO W6800");
    }

    #[test]
    fn readings_are_converted_to_the_units_the_page_uses() {
        let (_r, hw) = one_card();
        let s = hw.sample(0);
        assert_eq!(s.temp, Some(62));
        assert_eq!(s.fans, vec![Some(40)], "pwm 102 of 255");
        assert_eq!(s.power_w, Some(151.0));
        assert_eq!(s.power_limit_w, Some(203));
        assert_eq!((s.util, s.vram_used_mib, s.vram_total_mib), (Some(37), Some(2048), Some(16368)));
        assert_eq!(s.clock_mhz, Some(1500), "the active pp_dpm_sclk level");
        assert_eq!(hw.temp_c(0).unwrap(), 62);
        assert_eq!(hw.power_limit_w(0), Some(203));
    }

    #[test]
    fn the_power_cap_can_only_be_lowered_and_is_written_in_microwatts() {
        let (r, hw) = one_card();
        let mut d = daemon(hw);
        assert_eq!(d.cards[0].range, Some((100, 203)), "unknown card: lower only");
        let ok = d.handle(&json!({"op": "set_power", "uuid": "AMD-0000:03:00.0", "watts": 150}));
        assert_eq!(ok["ok"], json!(true), "{ok}");
        assert_eq!(read(&r.join("class/drm/card0/device/hwmon/hwmon3/power1_cap")).unwrap(), "150000000");
        let bad = d.handle(&json!({"op": "set_power", "uuid": "AMD-0000:03:00.0", "watts": 220}));
        assert_eq!(bad["ok"], json!(false));
        assert!(bad["error"].as_str().unwrap().contains("100-203 W"));
        assert_eq!(read(&r.join("class/drm/card0/device/hwmon/hwmon3/power1_cap")).unwrap(), "150000000", "rejected: nothing written");
    }

    #[test]
    fn a_card_that_reports_no_minimum_cannot_be_capped_to_zero() {
        let (r, _) = one_card();
        put(&r, "class/drm/card0/device/hwmon/hwmon3/power1_cap_min", "0\n");
        let hw = AmdHw::discover(&r, &r.join("pci.ids")).unwrap();
        assert_eq!(hw.gpus()[0].power_min_w, Some(101), "half the default");
    }

    #[test]
    fn fan_control_switches_to_manual_and_back_to_the_driver() {
        let (r, hw) = one_card();
        let mut d = daemon(hw);
        let h = r.join("class/drm/card0/device/hwmon/hwmon3");
        let resp = d.handle(&json!({"op": "set_fan", "uuid": "AMD-0000:03:00.0", "mode": "manual", "manual_pct": 60}));
        assert_eq!(resp["ok"], json!(true), "{resp}");
        assert_eq!(read(&h.join("pwm1_enable")).unwrap(), "1");
        assert_eq!(read(&h.join("pwm1")).unwrap(), "153", "60% of 255");
        d.handle(&json!({"op": "set_fan", "uuid": "AMD-0000:03:00.0", "mode": "auto"}));
        assert_eq!(read(&h.join("pwm1_enable")).unwrap(), "2");
        write(&h.join("pwm1_enable"), "1").unwrap();
        d.restore_fans();
        assert_eq!(read(&h.join("pwm1_enable")).unwrap(), "2", "stopping hands the fan back");
    }

    #[test]
    fn the_safety_floor_applies_to_amd_fans_too() {
        let (r, hw) = one_card();
        let mut d = daemon(hw);
        put(&r, "class/drm/card0/device/hwmon/hwmon3/temp1_input", "95000\n");
        d.handle(&json!({"op": "set_fan", "uuid": "AMD-0000:03:00.0", "mode": "manual", "manual_pct": 30}));
        assert_eq!(read(&r.join("class/drm/card0/device/hwmon/hwmon3/pwm1")).unwrap(), "255", "95 C is past the floor: full speed");
    }

    #[test]
    fn a_card_without_a_controllable_fan_offers_none() {
        let root = tmp();
        add_card(&root, "card0", "0x1002", "amdgpu", "0000:03:00.0", false);
        let hw = AmdHw::discover(&root, &root.join("none")).unwrap();
        assert_eq!(hw.gpus()[0].nfans, 0);
        let mut d = daemon(hw);
        let r = d.handle(&json!({"op": "set_fan", "uuid": "AMD-0000:03:00.0", "mode": "manual", "manual_pct": 50}));
        assert!(r["error"].as_str().unwrap().contains("no controllable fans"));
    }

    #[test]
    fn clock_caps_are_not_offered() {
        let (_r, hw) = one_card();
        let mut d = daemon(hw);
        assert_eq!(d.status()["gpus"][0]["clock_range"], serde_json::Value::Null);
        let r = d.handle(&json!({"op": "set_clock_cap", "uuid": "AMD-0000:03:00.0", "mhz": 2000}));
        assert_eq!(r["ok"], json!(false));
    }

    #[test]
    fn without_a_critical_temperature_the_default_floor_is_used() {
        let (r, _) = one_card();
        fs::remove_file(r.join("class/drm/card0/device/hwmon/hwmon3/temp1_crit")).unwrap();
        let hw = AmdHw::discover(&r, &r.join("pci.ids")).unwrap();
        assert_eq!(hw.gpus()[0].slowdown(), None);
    }

    #[test]
    fn dpm_levels_parse_the_active_and_top_clock() {
        let r = tmp();
        put(&r, "s", "0: 300Mhz\n1: 900Mhz *\n2: 1800Mhz\n");
        assert_eq!(dpm_levels(&r.join("s")), (Some(900), Some(1800)));
        assert_eq!(dpm_levels(&r.join("missing")), (None, None));
    }

    #[test]
    fn the_probe_report_lists_present_and_missing_files_and_never_writes() {
        let (r, _) = one_card();
        put(&r, "class/drm/card0/device/gpu_od/fan_ctrl/fan_curve", "x\n");
        let before = read(&r.join("class/drm/card0/device/hwmon/hwmon3/power1_cap")).unwrap();
        let report = diagnose(&r);
        assert!(report.contains("[card0] AMD device 0x73bf  driver=amdgpu  slot=0000:03:00.0"), "{report}");
        assert!(report.contains("power1_cap: 203000000"), "{report}");
        assert!(report.contains("pwm1: 102"), "{report}");
        assert!(report.contains("temp3_input: (missing)"), "{report}");
        assert!(report.contains("gpu_od/: fan_ctrl"), "{report}");
        assert!(report.contains("pp_dpm_sclk: 0: 500Mhz | 1: 1500Mhz * | 2: 2400Mhz"), "{report}");
        assert_eq!(read(&r.join("class/drm/card0/device/hwmon/hwmon3/power1_cap")).unwrap(), before);
        assert!(diagnose(&tmp()).contains("no AMD GPU found"));
        add_card(&r, "card1", "0x1002", "amdgpu", "0000:04:00.0", false);
        assert!(diagnose(&r).contains("pwm1: (missing)"), "a card without pwm is reported as such");
    }

    #[test]
    fn amd_and_nvidia_cards_can_run_side_by_side() {
        use crate::fake::FakeHw;
        use crate::multi::Multi;
        let (r, amd) = one_card();
        let multi = Multi::new(vec![Arc::new(FakeHw::pair()), Arc::new(amd)]);
        assert_eq!(multi.gpus().len(), 3);
        assert_eq!(multi.gpus()[2].index, 2);
        assert_eq!((multi.gpus()[0].vendor, multi.gpus()[2].vendor), ("nvidia", "amd"));
        multi.set_power_limit_w(2, 120).unwrap();
        assert_eq!(read(&r.join("class/drm/card0/device/hwmon/hwmon3/power1_cap")).unwrap(), "120000000");
        assert_eq!(multi.power_limit_w(2), Some(120));
        assert_eq!(multi.power_limit_w(0), Some(300), "the NVIDIA cards are untouched");
        assert_eq!(multi.temp_c(1).unwrap(), 35);
    }
}
