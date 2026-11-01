//! A fake GPU for tests and for `gputuner run --fake`. Nothing here touches hardware.

use crate::hw::*;
use std::collections::BTreeMap;
use std::sync::Mutex;

pub const MAXQ: &str = "GPU-maxq-0000";
pub const WS: &str = "GPU-ws-0000";
pub const GB10: &str = "GPU-gb10-0000";

#[derive(Debug, Clone)]
pub struct FakeCard {
    pub uuid: String,
    pub name: String,
    pub fans: u32,
    pub limit_w: Option<i64>,
    pub constraints: Option<(i64, i64)>,
    pub default_w: Option<i64>,
    pub temp: i64,
    pub slowdown: i64,
    pub power_supported: bool,
    pub persistence_supported: bool,
    pub clocks: Vec<i64>,
}

#[derive(Debug, Default)]
pub struct FakeState {
    pub cards: Vec<FakeCard>,
    pub fan_writes: Vec<(usize, u32, i64)>,
    pub fan_auto: Vec<(usize, u32)>,
    pub clock_calls: Vec<(usize, Option<(i64, i64)>)>,
    pub power_writes: Vec<(usize, i64)>,
    pub refuse_fan: bool,
    pub refuse_temp: bool,
    pub refuse_auto: bool,
}

pub struct FakeHw {
    infos: Vec<GpuInfo>,
    pub state: Mutex<FakeState>,
    driver: String,
}

fn clocks() -> Vec<i64> {
    (180..=3090).step_by(15).collect()
}

fn card(uuid: &str, name: &str, fans: u32, limit: i64, con: (i64, i64), default: i64, temp: i64) -> FakeCard {
    FakeCard {
        uuid: uuid.into(),
        name: name.into(),
        fans,
        limit_w: Some(limit),
        constraints: Some(con),
        default_w: Some(default),
        temp,
        slowdown: 95,
        power_supported: true,
        persistence_supported: true,
        clocks: clocks(),
    }
}

impl FakeHw {
    pub fn from_cards(cards: Vec<FakeCard>) -> Self {
        let infos = cards
            .iter()
            .enumerate()
            .map(|(i, c)| GpuInfo {
                index: i,
                uuid: c.uuid.clone(),
                name: c.name.clone(),
                pci: format!("00000000:0{}:00.0", i + 1),
                vbios: "98.02".into(),
                nfans: c.fans,
                fan_min: 30,
                fan_max: 100,
                power_min_w: c.constraints.map(|x| x.0),
                power_max_w: c.constraints.map(|x| x.1),
                power_default_w: c.default_w,
                thresholds: BTreeMap::from([
                    ("shutdown".to_string(), c.slowdown + 3),
                    ("slowdown".to_string(), c.slowdown),
                    ("t_limit".to_string(), c.slowdown - 3),
                ]),
                mem_kind: "dedicated".into(),
                clocks: c.clocks.clone(),
            })
            .collect();
        FakeHw { infos, state: Mutex::new(FakeState { cards, ..Default::default() }), driver: "595.84".into() }
    }

    /// Two RTX PRO 6000 cards, Max-Q + Workstation: the original upstream test fixture.
    pub fn pair() -> Self {
        Self::from_cards(vec![
            card(MAXQ, "NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition", 1, 300, (250, 325), 300, 40),
            card(WS, "NVIDIA RTX PRO 6000 Blackwell Workstation Edition", 2, 450, (150, 600), 600, 35),
        ])
    }

    /// One GeForce-like card for `--fake` demos.
    pub fn demo() -> Self {
        Self::from_cards(vec![card("GPU-demo-0000", "NVIDIA GeForce RTX 3070", 2, 220, (100, 220), 220, 52)])
    }

    /// A DGX Spark-like SoC GPU: no power limit, no fans, no clock list, persistence unsupported.
    pub fn gb10() -> Self {
        let mut c = card(GB10, "NVIDIA GB10", 0, 0, (0, 0), 0, 52);
        c.limit_w = None;
        c.constraints = None;
        c.default_w = None;
        c.slowdown = 86;
        c.power_supported = false;
        c.persistence_supported = false;
        c.clocks = vec![];
        Self::from_cards(vec![c])
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }
}

fn err(msg: &str) -> HwError {
    HwError::new(msg)
}

impl Hw for FakeHw {
    fn driver(&self) -> String {
        self.driver.clone()
    }
    fn gpus(&self) -> &[GpuInfo] {
        &self.infos
    }
    fn temp_c(&self, i: usize) -> HResult<i64> {
        let s = self.state.lock().unwrap();
        if s.refuse_temp {
            return Err(err("Unknown Error"));
        }
        Ok(s.cards[i].temp)
    }
    fn power_limit_w(&self, i: usize) -> Option<i64> {
        self.state.lock().unwrap().cards[i].limit_w
    }
    fn sample(&self, i: usize) -> Sample {
        let s = self.state.lock().unwrap();
        let c = &s.cards[i];
        Sample {
            temp: Some(c.temp),
            fans: (0..c.fans).map(|_| Some(45)).collect(),
            power_w: c.limit_w.map(|l| l as f64 / 2.0),
            power_limit_w: c.limit_w,
            util: Some(50),
            mem_util: Some(20),
            vram_used_mib: Some(10 * 1024),
            vram_total_mib: Some(96 * 1024),
            mem_kind: "dedicated".into(),
            clock_mhz: Some(1800),
            mem_clock_mhz: Some(14001),
            pstate: Some(2),
            reasons: vec![],
        }
    }
    fn set_persistence(&self, i: usize) -> HResult<()> {
        if self.state.lock().unwrap().cards[i].persistence_supported {
            Ok(())
        } else {
            Err(HwError::with_code(NOT_SUPPORTED, "Not Supported"))
        }
    }
    fn set_power_limit_w(&self, i: usize, watts: i64) -> HResult<()> {
        let mut s = self.state.lock().unwrap();
        s.power_writes.push((i, watts));
        if !s.cards[i].power_supported {
            return Err(HwError::with_code(NOT_SUPPORTED, "Not Supported"));
        }
        s.cards[i].limit_w = Some(watts);
        Ok(())
    }
    fn set_fan_pct(&self, i: usize, pct: i64) -> HResult<()> {
        let mut s = self.state.lock().unwrap();
        if s.refuse_fan {
            return Err(err("Insufficient Permissions"));
        }
        for f in 0..s.cards[i].fans {
            s.fan_writes.push((i, f, pct));
        }
        Ok(())
    }
    fn set_fan_auto(&self, i: usize) -> HResult<()> {
        let mut s = self.state.lock().unwrap();
        for f in 0..s.cards[i].fans {
            s.fan_auto.push((i, f));
        }
        if s.refuse_auto {
            return Err(err("Insufficient Permissions"));
        }
        Ok(())
    }
    fn set_clock_cap(&self, i: usize, mhz: i64) -> HResult<()> {
        let mut s = self.state.lock().unwrap();
        let lo = s.cards[i].clocks.iter().copied().min().unwrap_or(0);
        s.clock_calls.push((i, Some((lo, mhz))));
        Ok(())
    }
    fn reset_clock_cap(&self, i: usize) -> HResult<()> {
        self.state.lock().unwrap().clock_calls.push((i, None));
        Ok(())
    }
}
