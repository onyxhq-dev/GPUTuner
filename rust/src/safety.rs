//! The safety envelope: pure functions, no hardware, no I/O, so every limit here is unit tested.
//!
//! Ported from gpu_tuner/safety.py of github.com/willboyden/gpu-tuner (MIT, see LICENSE-gpu-tuner).
//!
//! Three layers, innermost first:
//!   1. The card's own range as NVML reports it live.
//!   2. The known-good profile for that exact product, intersected with (1). A card with no
//!      profile may be lowered but never raised above its factory default.
//!   3. A combined GPU power budget across all cards of the machine (soft, can be confirmed).

use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum SafetyErr {
    Msg(String),
    Budget { msg: String, total_w: i64, budget_w: i64, others_w: i64 },
}

impl SafetyErr {
    pub fn message(&self) -> &str {
        match self {
            SafetyErr::Msg(m) => m,
            SafetyErr::Budget { msg, .. } => msg,
        }
    }
}

impl std::fmt::Display for SafetyErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

pub type SResult<T> = Result<T, SafetyErr>;

fn bad<T>(m: impl Into<String>) -> SResult<T> {
    Err(SafetyErr::Msg(m.into()))
}

pub const FAN_FULL_BY_C: i64 = 85;
pub const FAN_FLOOR_MARGIN_C: i64 = 10;
pub const CURVE_TEMP_MIN_C: i64 = 20;
pub const CURVE_MIN_POINTS: usize = 2;
pub const CURVE_MAX_POINTS: usize = 8;
pub const FAN_ABS_MIN_PCT: i64 = 30;
pub const HYSTERESIS_C: i64 = 2;
pub const CLOCK_CAP_MIN_MHZ: i64 = 1000;
pub const DEFAULT_GPU_BUDGET_W: i64 = 750;

const SAFETY_FLOOR_SHAPE: [(i64, i64); 5] = [(50, 30), (60, 43), (70, 52), (80, 65), (FAN_FULL_BY_C, 100)];
pub const LAB_CURVE: [(i64, i64); 6] = [(30, 30), (45, 45), (55, 60), (65, 75), (72, 88), (80, 100)];

pub struct CurvePreset {
    pub key: &'static str,
    pub label: &'static str,
    pub note: &'static str,
    pub curve: &'static [(i64, i64)],
}

pub const CURVE_PRESETS: [CurvePreset; 3] = [
    CurvePreset {
        key: "lab",
        label: "Lab (cooling-first)",
        curve: &LAB_CURVE,
        note: "A cooling-first curve: 100% at 80 C.",
    },
    CurvePreset {
        key: "balanced",
        label: "Balanced",
        curve: &[(40, 30), (55, 45), (65, 60), (75, 80), (83, 100)],
        note: "Quieter at idle and mid-load; still full speed by 83 C.",
    },
    CurvePreset {
        key: "quiet",
        label: "Quiet",
        curve: &[(50, 30), (60, 45), (70, 55), (80, 70), (85, 100)],
        note: "Rides just above the safety floor. Expect temperatures near the stock curve's \
               (high 70s to mid 80s under load), which costs 4-8% of clock.",
    },
];

pub struct PowerPreset {
    pub w: i64,
    pub label: &'static str,
    pub note: &'static str,
}

pub struct Profile {
    pub matcher: &'static str,
    pub label: &'static str,
    pub power_min_w: i64,
    pub power_max_w: i64,
    pub baseline_power_w: i64,
    pub presets: &'static [PowerPreset],
    pub note: &'static str,
}

pub static PROFILES: [Profile; 2] = [
    Profile {
        matcher: "RTX PRO 6000 Blackwell Max-Q Workstation Edition",
        label: "Max-Q",
        power_min_w: 250,
        power_max_w: 325,
        baseline_power_w: 300,
        presets: &[
            PowerPreset { w: 250, label: "250 W floor", note: "Measured: 1,822 MHz at 71 C under load." },
            PowerPreset {
                w: 300,
                label: "300 W default",
                note: "Factory default and lab baseline. Measured: 2,145 MHz at 75 C (+17% clock over 250 W).",
            },
            PowerPreset {
                w: 325,
                label: "325 W ceiling",
                note: "Not yet measured here; the findings note predicts low-to-mid single-digit % more clock.",
            },
        ],
        note: "The 600 W GB202 die held to 300 W: under sustained load it sits at its cap by design, \
               and the cap is its only real performance lever.",
    },
    Profile {
        matcher: "RTX PRO 6000 Blackwell Workstation Edition",
        label: "Workstation",
        power_min_w: 150,
        power_max_w: 600,
        baseline_power_w: 450,
        presets: &[
            PowerPreset {
                w: 300,
                label: "300 W efficient",
                note: "Not measured here. Deep in the efficient part of the voltage/frequency curve.",
            },
            PowerPreset {
                w: 450,
                label: "450 W lab cap",
                note: "Lab baseline. Measured: 1,980 MHz at 73 C; renders flat-top here by choice.",
            },
            PowerPreset {
                w: 525,
                label: "525 W",
                note: "Measured: 2,130 MHz at 78 C (+7.6% clock for +16.7% power). Needs budget headroom.",
            },
            PowerPreset {
                w: 600,
                label: "600 W factory",
                note: "Factory default. Needs budget headroom the documented 750 W ceiling does not have.",
            },
        ],
        note: "Already well up its voltage/frequency curve at 450 W, so extra watts buy less clock \
               than they do on the Max-Q.",
    },
];

pub fn profile_for(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| name.ends_with(p.matcher))
}

/// A whole number from JSON: bools, strings, null, fractions and non-finite values are rejected.
pub fn as_int(v: &Value, what: &str) -> SResult<i64> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Ok(i);
            }
            match n.as_f64() {
                Some(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15 => Ok(f as i64),
                _ => bad(format!("{what} must be a whole number")),
            }
        }
        _ => bad(format!("{what} must be a whole number")),
    }
}

/// (curve, full_by_c) for one card: the measured floor shape, rescaled to that card's own
/// slowdown threshold. Falls back to the fixed curve when the threshold is unknown.
pub fn safety_floor_for(slowdown_c: Option<i64>) -> (Vec<(f64, i64)>, i64) {
    match slowdown_c {
        Some(s) if s != 0 => {
            let full_by = s - FAN_FLOOR_MARGIN_C;
            let scale = full_by as f64 / SAFETY_FLOOR_SHAPE[SAFETY_FLOOR_SHAPE.len() - 1].0 as f64;
            (SAFETY_FLOOR_SHAPE.iter().map(|&(t, p)| (t as f64 * scale, p)).collect(), full_by)
        }
        _ => (SAFETY_FLOOR_SHAPE.iter().map(|&(t, p)| (t as f64, p)).collect(), FAN_FULL_BY_C),
    }
}

pub fn default_curve(full_by: i64) -> Vec<(i64, i64)> {
    let last = LAB_CURVE[LAB_CURVE.len() - 1].0;
    if full_by >= last {
        return LAB_CURVE.to_vec();
    }
    let mut out: Vec<(i64, i64)> = Vec::new();
    let mut prev = CURVE_TEMP_MIN_C - 1;
    for &(t, pct) in LAB_CURVE.iter() {
        let t2 = (prev + 1).max(CURVE_TEMP_MIN_C).max(t * full_by / last);
        out.push((t2, pct));
        prev = t2;
    }
    let n = out.len();
    out[n - 1] = (full_by, 100);
    if out[n - 2].0 < full_by {
        out
    } else {
        vec![(CURVE_TEMP_MIN_C, out[0].1), (full_by, 100)]
    }
}

pub fn power_range(
    profile: Option<&Profile>,
    nvml_min_w: Option<i64>,
    nvml_max_w: Option<i64>,
    nvml_default_w: Option<i64>,
) -> Option<(i64, i64)> {
    let hi0 = nvml_max_w.filter(|&m| m > 0)?;
    let lo0 = nvml_min_w?;
    let (lo, hi) = match profile {
        None => {
            let d = nvml_default_w.filter(|&d| d > 0)?;
            (lo0, hi0.min(d))
        }
        Some(p) => (lo0.max(p.power_min_w), hi0.min(p.power_max_w)),
    };
    if lo <= hi {
        Some((lo, hi))
    } else {
        None
    }
}

/// Validate one card's new limit against its range and the combined budget. `ranges` and
/// `current` cover every card, so the budget sees the whole machine.
pub fn check_power(
    uuid: &str,
    watts: &Value,
    ranges: &BTreeMap<String, Option<(i64, i64)>>,
    current: &BTreeMap<String, i64>,
    budget_w: Option<i64>,
    confirm_override: bool,
) -> SResult<i64> {
    let watts = as_int(watts, "power limit")?;
    let (lo, hi) = match ranges.get(uuid).copied().flatten() {
        Some(r) => r,
        None => return bad("this card has no settable power range"),
    };
    if !(lo <= watts && watts <= hi) {
        return bad(format!("power limit must be {lo}-{hi} W for this card"));
    }
    let others_w: i64 = current.iter().filter(|(u, _)| u.as_str() != uuid).map(|(_, w)| *w).sum();
    let total = others_w + watts;
    if let Some(budget) = budget_w {
        if total > budget && watts > current.get(uuid).copied().unwrap_or(0) && !confirm_override {
            let n_others = current.keys().filter(|u| u.as_str() != uuid).count();
            let msg = if n_others == 0 {
                format!(
                    "{watts} W is {} W over this machine's {budget} W GPU budget. Raise the budget, or confirm to exceed it anyway.",
                    total - budget
                )
            } else {
                let lead = if n_others == 1 {
                    format!("{watts} W here plus {others_w} W on the other card is")
                } else {
                    format!("{watts} W here plus {others_w} W across the other {n_others} cards is")
                };
                format!(
                    "{lead} {total} W, over the {budget} W combined GPU budget. Lower another card first, raise the budget, or confirm to exceed it anyway."
                )
            };
            return Err(SafetyErr::Budget { msg, total_w: total, budget_w: budget, others_w });
        }
    }
    Ok(watts)
}

pub fn interp(curve: &[(f64, f64)], temp: f64) -> f64 {
    if temp <= curve[0].0 {
        return curve[0].1;
    }
    let last = curve[curve.len() - 1];
    if temp >= last.0 {
        return last.1;
    }
    for w in curve.windows(2) {
        let (t0, f0) = w[0];
        let (t1, f1) = w[1];
        if t0 <= temp && temp <= t1 {
            return f0 + (f1 - f0) * ((temp - t0) / (t1 - t0));
        }
    }
    last.1
}

pub fn floor_pct(temp: i64, slowdown_c: Option<i64>, fan_min: i64) -> i64 {
    let (curve, _) = safety_floor_for(slowdown_c);
    let c: Vec<(f64, f64)> = curve.iter().map(|&(t, p)| (t, p as f64)).collect();
    fan_min.max(interp(&c, temp as f64).ceil() as i64)
}

/// Normalise a user curve to [[temp, pct], ...] or explain what is wrong with it.
pub fn validate_curve(points: &Value, full_by: i64, fan_min: i64) -> SResult<Vec<[i64; 2]>> {
    let arr = match points.as_array() {
        Some(a) => a,
        None => return bad("curve must be a list of [temperature, fan %] points"),
    };
    if !(CURVE_MIN_POINTS <= arr.len() && arr.len() <= CURVE_MAX_POINTS) {
        return bad(format!("curve needs {CURVE_MIN_POINTS}-{CURVE_MAX_POINTS} points"));
    }
    let mut out: Vec<[i64; 2]> = Vec::new();
    for p in arr {
        let pair = match p.as_array() {
            Some(a) if a.len() == 2 => a,
            _ => return bad("each curve point is [temperature, fan %]"),
        };
        let t = as_int(&pair[0], "curve temperature")?;
        let f = as_int(&pair[1], "curve fan %")?;
        if !(CURVE_TEMP_MIN_C <= t && t <= full_by) {
            return bad(format!("curve temperatures must be {CURVE_TEMP_MIN_C}-{full_by} C"));
        }
        if !(fan_min <= f && f <= 100) {
            return bad(format!(
                "curve fan speeds must be {fan_min}-100% (the card's fan minimum is {fan_min}%)"
            ));
        }
        if let Some(last) = out.last() {
            if t <= last[0] {
                return bad("curve temperatures must strictly increase");
            }
            if f < last[1] {
                return bad("fan speed must never fall as temperature rises");
            }
        }
        out.push([t, f]);
    }
    if out[out.len() - 1][1] != 100 {
        return bad(format!("the last curve point must be 100% (at or below {full_by} C)"));
    }
    Ok(out)
}

pub fn validate_manual(pct: &Value, fan_min: i64) -> SResult<i64> {
    let p = as_int(pct, "fixed fan speed")?;
    if !(fan_min <= p && p <= 100) {
        return bad(format!("fixed fan speed must be {fan_min}-100%"));
    }
    Ok(p)
}

/// (percent, floor_active) for the 'curve' and 'manual' modes.
pub fn fan_target(
    mode: &str,
    temp: i64,
    curve: &[[i64; 2]],
    manual_pct: i64,
    slowdown_c: Option<i64>,
    fan_min: i64,
) -> SResult<(i64, bool)> {
    let floor = floor_pct(temp, slowdown_c, fan_min);
    let want = match mode {
        "curve" => {
            let c: Vec<(f64, f64)> = curve.iter().map(|p| (p[0] as f64, p[1] as f64)).collect();
            interp(&c, temp as f64).round_ties_even() as i64
        }
        "manual" => manual_pct,
        other => return bad(format!("unknown fan mode '{other}'")),
    };
    let want = fan_min.max(100.min(want));
    Ok(if floor > want { (floor, true) } else { (want, false) })
}

/// `prev` is (pct, temp_when_set). Never returns less than target, so it can hold the fans
/// higher than the curve for a moment but never push them under the floor.
pub fn hysteresis(target: i64, temp: i64, prev: Option<(i64, i64)>) -> i64 {
    match prev {
        None => target,
        Some((prev_pct, prev_temp)) => {
            if target >= prev_pct || temp <= prev_temp - HYSTERESIS_C {
                target
            } else {
                prev_pct
            }
        }
    }
}

/// None clears the cap. Otherwise snap DOWN to a clock the card actually supports.
pub fn validate_clock_cap(mhz: &Value, supported: &[i64]) -> SResult<Option<i64>> {
    if mhz.is_null() {
        return Ok(None);
    }
    let mhz = as_int(mhz, "clock cap")?;
    if supported.is_empty() {
        return bad("this card reports no supported clock list");
    }
    let top = *supported.iter().max().unwrap();
    let lo = if top >= CLOCK_CAP_MIN_MHZ { CLOCK_CAP_MIN_MHZ } else { *supported.iter().min().unwrap() };
    if !(lo <= mhz && mhz <= top) {
        return bad(format!("clock cap must be {lo}-{top} MHz"));
    }
    let below = supported.iter().copied().filter(|&c| c <= mhz).max();
    Ok(Some(below.unwrap_or_else(|| *supported.iter().min().unwrap())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MAXQ: &str = "GPU-maxq-0000";
    const WS: &str = "GPU-ws-0000";

    fn ranges() -> BTreeMap<String, Option<(i64, i64)>> {
        BTreeMap::from([(MAXQ.into(), Some((250, 325))), (WS.into(), Some((150, 600)))])
    }
    fn cur(a: i64, b: i64) -> BTreeMap<String, i64> {
        BTreeMap::from([(MAXQ.into(), a), (WS.into(), b)])
    }

    #[test]
    fn profile_narrows_to_card_range() {
        let p = profile_for("NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition");
        assert_eq!(power_range(p, Some(100), Some(999), Some(300)), Some((250, 325)));
    }

    #[test]
    fn unknown_card_can_only_be_lowered() {
        assert_eq!(power_range(None, Some(100), Some(500), Some(350)), Some((100, 350)));
        assert_eq!(power_range(None, Some(100), Some(500), None), None);
    }

    #[test]
    fn no_reported_range_is_none_not_zero() {
        assert_eq!(power_range(None, None, None, None), None);
        assert_eq!(power_range(None, Some(0), Some(0), Some(0)), None);
        assert_eq!(power_range(None, None, Some(0), None), None);
        let p = profile_for("NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition");
        assert_eq!(power_range(p, None, None, None), None);
    }

    #[test]
    fn in_range_and_in_budget_ok() {
        assert_eq!(check_power(MAXQ, &json!(325), &ranges(), &cur(300, 425), Some(750), false), Ok(325));
    }

    #[test]
    fn over_card_range_rejected() {
        assert!(check_power(MAXQ, &json!(326), &ranges(), &cur(300, 450), Some(750), false).is_err());
        assert!(check_power(MAXQ, &json!(249), &ranges(), &cur(300, 450), Some(750), false).is_err());
    }

    #[test]
    fn over_budget_by_one_watt_is_a_distinguishable_error() {
        match check_power(WS, &json!(451), &ranges(), &cur(300, 450), Some(750), false) {
            Err(SafetyErr::Budget { msg, total_w, budget_w, others_w }) => {
                assert_eq!((total_w, budget_w, others_w), (751, 750, 300));
                assert!(msg.contains("751 W"));
                assert!(msg.contains("on the other card is"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn confirm_override_allows_budget_but_never_the_cards_own_range() {
        assert_eq!(check_power(WS, &json!(451), &ranges(), &cur(300, 450), Some(750), true), Ok(451));
        assert!(check_power(WS, &json!(601), &ranges(), &cur(300, 450), Some(750), true).is_err());
    }

    #[test]
    fn over_budget_message_scales_with_card_count() {
        let r1 = BTreeMap::from([(WS.to_string(), Some((150, 900)))]);
        let e = check_power(WS, &json!(900), &r1, &BTreeMap::new(), Some(750), false).unwrap_err();
        assert_eq!(
            e.message(),
            "900 W is 150 W over this machine's 750 W GPU budget. Raise the budget, or confirm to exceed it anyway."
        );
        let e = check_power(WS, &json!(900), &r1, &BTreeMap::from([(WS.to_string(), 800)]), Some(750), false)
            .unwrap_err();
        assert!(!e.message().contains("other card"));
        let mut r3 = ranges();
        r3.insert("THIRD".into(), Some((100, 900)));
        let mut c3 = cur(300, 450);
        c3.insert("THIRD".into(), 100);
        let e = check_power(WS, &json!(451), &r3, &c3, Some(750), false).unwrap_err();
        assert!(e.message().contains("across the other 2 cards is"));
    }

    #[test]
    fn lowering_is_allowed_even_when_over_budget() {
        assert_eq!(check_power(WS, &json!(550), &ranges(), &cur(325, 600), Some(750), false), Ok(550));
    }

    #[test]
    fn bool_fraction_string_and_null_rejected() {
        for bad in [json!(true), json!(300.5), json!("300"), json!(null)] {
            assert!(check_power(MAXQ, &bad, &ranges(), &cur(300, 450), Some(750), false).is_err(), "{bad}");
        }
        assert_eq!(check_power(MAXQ, &json!(300.0), &ranges(), &cur(300, 450), Some(750), false), Ok(300));
    }

    #[test]
    fn no_budget_means_no_budget_check() {
        assert_eq!(check_power(WS, &json!(600), &ranges(), &cur(300, 450), None, false), Ok(600));
    }

    #[test]
    fn lab_curve_and_every_preset_validate() {
        let lab: Vec<Value> = LAB_CURVE.iter().map(|&(t, p)| json!([t, p])).collect();
        assert!(validate_curve(&Value::Array(lab), FAN_FULL_BY_C, 30).is_ok());
        for p in CURVE_PRESETS.iter() {
            let c: Vec<Value> = p.curve.iter().map(|&(t, f)| json!([t, f])).collect();
            assert!(validate_curve(&Value::Array(c), FAN_FULL_BY_C, 30).is_ok(), "{}", p.key);
        }
    }

    #[test]
    fn bad_curves_rejected() {
        let v = |x: Value| validate_curve(&x, FAN_FULL_BY_C, 30);
        assert!(v(json!([[30, 50], [60, 40], [80, 100]])).is_err(), "falling");
        assert!(v(json!([[30, 30], [80, 90]])).is_err(), "must end at 100");
        assert!(v(json!([[30, 30], [86, 100]])).is_err(), "past full_by");
        assert!(v(json!([[30, 30], [85, 100]])).is_ok());
        assert!(v(json!([[30, 29], [80, 100]])).is_err(), "below fan minimum");
        assert!(v(json!([[80, 100]])).is_err(), "one point");
        assert!(v(json!([[40, 40], [40, 50], [80, 100]])).is_err(), "not increasing");
        let nine: Vec<Value> = (0..9).map(|i| json!([20 + i, 30 + i])).collect();
        assert!(v(Value::Array(nine)).is_err(), "too many points");
    }

    #[test]
    fn manual_speed_bounds() {
        assert!(validate_manual(&json!(29), 30).is_err());
        assert_eq!(validate_manual(&json!(30), 30), Ok(30));
        assert!(validate_manual(&json!(101), 30).is_err());
    }

    #[test]
    fn floor_is_never_lazier_than_measured_stock() {
        assert!(floor_pct(61, None, 30) >= 43);
        assert!(floor_pct(78, None, 30) >= 46);
        assert!(floor_pct(80, None, 30) >= 59);
        assert_eq!(floor_pct(FAN_FULL_BY_C, None, 30), 100);
        assert_eq!(floor_pct(99, None, 30), 100);
    }

    #[test]
    fn safety_floor_for_is_a_noop_at_95_and_rescales_otherwise() {
        let (curve, full_by) = safety_floor_for(Some(95));
        assert_eq!(full_by, 85);
        let shape: Vec<(f64, i64)> = SAFETY_FLOOR_SHAPE.iter().map(|&(t, p)| (t as f64, p)).collect();
        assert_eq!(curve, shape);
        let (curve, full_by) = safety_floor_for(None);
        assert_eq!((curve, full_by), (shape, 85));
        let (curve, full_by) = safety_floor_for(Some(105));
        assert_eq!(full_by, 95);
        assert_eq!(curve[curve.len() - 1].0, 95.0);
        assert!(curve[0].0 > 50.0);
        assert_eq!(floor_pct(95, Some(105), 30), 100);
        assert!(floor_pct(90, Some(105), 30) < floor_pct(90, None, 30));
    }

    #[test]
    fn quiet_preset_and_fixed_speed_are_lifted_by_the_floor() {
        let quiet: Vec<[i64; 2]> = CURVE_PRESETS[2].curve.iter().map(|&(t, p)| [t, p]).collect();
        let (pct, _) = fan_target("curve", 75, &quiet, 0, None, 30).unwrap();
        assert!(pct >= floor_pct(75, None, 30));
        let (pct, floored) = fan_target("manual", 84, &[], 30, None, 30).unwrap();
        assert!(floored && pct >= 90);
        assert_eq!(fan_target("manual", 40, &[], 30, None, 30).unwrap(), (30, false));
        assert!(fan_target("auto", 40, &[], 30, None, 30).is_err());
    }

    #[test]
    fn hysteresis_holds_on_the_way_down_only() {
        assert_eq!(hysteresis(60, 70, None), 60);
        assert_eq!(hysteresis(70, 72, Some((60, 70))), 70);
        assert_eq!(hysteresis(58, 69, Some((60, 70))), 60);
        assert_eq!(hysteresis(58, 68, Some((60, 70))), 58);
        assert!(hysteresis(55, 69, Some((60, 70))) >= 55);
    }

    #[test]
    fn clock_cap_snaps_down_and_checks_bounds() {
        let sup: Vec<i64> = (180..=3090).step_by(15).collect();
        assert_eq!(validate_clock_cap(&json!(2001), &sup), Ok(Some(1995)));
        assert_eq!(validate_clock_cap(&json!(3090), &sup), Ok(Some(3090)));
        assert_eq!(validate_clock_cap(&json!(null), &sup), Ok(None));
        assert!(validate_clock_cap(&json!(999), &sup).is_err());
        assert!(validate_clock_cap(&json!(3091), &sup).is_err());
        let low = [180, 400, 620, 800];
        assert_eq!(validate_clock_cap(&json!(700), &low), Ok(Some(620)));
        assert_eq!(validate_clock_cap(&json!(800), &low), Ok(Some(800)));
        assert!(validate_clock_cap(&json!(801), &low).is_err());
    }

    #[test]
    fn default_curve_keeps_the_lab_shape_and_ends_at_full_by() {
        assert_eq!(default_curve(85), LAB_CURVE.to_vec());
        let c = default_curve(76);
        assert_eq!(c[c.len() - 1], (76, 100));
        assert!(c.windows(2).all(|w| w[0].0 < w[1].0));
        let as_json: Vec<Value> = c.iter().map(|&(t, p)| json!([t, p])).collect();
        assert!(validate_curve(&Value::Array(as_json), 76, 30).is_ok());
    }
}
