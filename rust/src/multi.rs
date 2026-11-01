//! Several hardware backends presented as one list of cards (for machines with both NVIDIA and
//! AMD GPUs). Card indexes are renumbered globally; every call is forwarded to the owning backend.

use crate::hw::*;
use std::sync::Arc;

pub struct Multi {
    parts: Vec<Arc<dyn Hw>>,
    infos: Vec<GpuInfo>,
    map: Vec<(usize, usize)>,
    driver: String,
}

impl Multi {
    pub fn new(parts: Vec<Arc<dyn Hw>>) -> Multi {
        let mut infos = Vec::new();
        let mut map = Vec::new();
        for (p, part) in parts.iter().enumerate() {
            for (local, g) in part.gpus().iter().enumerate() {
                let mut g = g.clone();
                g.index = infos.len();
                infos.push(g);
                map.push((p, local));
            }
        }
        let driver = parts.iter().map(|p| p.driver()).collect::<Vec<_>>().join(" + ");
        Multi { parts, infos, map, driver }
    }

    fn at(&self, i: usize) -> (&dyn Hw, usize) {
        let (p, local) = self.map[i];
        (self.parts[p].as_ref(), local)
    }
}

impl Hw for Multi {
    fn driver(&self) -> String {
        self.driver.clone()
    }
    fn gpus(&self) -> &[GpuInfo] {
        &self.infos
    }
    fn temp_c(&self, i: usize) -> HResult<i64> {
        let (h, l) = self.at(i);
        h.temp_c(l)
    }
    fn power_limit_w(&self, i: usize) -> Option<i64> {
        let (h, l) = self.at(i);
        h.power_limit_w(l)
    }
    fn sample(&self, i: usize) -> Sample {
        let (h, l) = self.at(i);
        h.sample(l)
    }
    fn set_persistence(&self, i: usize) -> HResult<()> {
        let (h, l) = self.at(i);
        h.set_persistence(l)
    }
    fn set_power_limit_w(&self, i: usize, watts: i64) -> HResult<()> {
        let (h, l) = self.at(i);
        h.set_power_limit_w(l, watts)
    }
    fn set_fan_pct(&self, i: usize, pct: i64) -> HResult<()> {
        let (h, l) = self.at(i);
        h.set_fan_pct(l, pct)
    }
    fn set_fan_auto(&self, i: usize) -> HResult<()> {
        let (h, l) = self.at(i);
        h.set_fan_auto(l)
    }
    fn set_clock_cap(&self, i: usize, mhz: i64) -> HResult<()> {
        let (h, l) = self.at(i);
        h.set_clock_cap(l, mhz)
    }
    fn reset_clock_cap(&self, i: usize) -> HResult<()> {
        let (h, l) = self.at(i);
        h.reset_clock_cap(l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeHw, MAXQ, WS};

    #[test]
    fn cards_are_renumbered_and_calls_reach_the_owning_backend() {
        let a = Arc::new(FakeHw::pair());
        let b = Arc::new(FakeHw::demo());
        let m = Multi::new(vec![a.clone(), b.clone()]);
        let uuids: Vec<&str> = m.gpus().iter().map(|g| g.uuid.as_str()).collect();
        assert_eq!(uuids, vec![MAXQ, WS, "GPU-demo-0000"]);
        assert_eq!(m.gpus().iter().map(|g| g.index).collect::<Vec<_>>(), vec![0, 1, 2]);
        m.set_power_limit_w(2, 150).unwrap();
        assert_eq!(b.with(|s| s.cards[0].limit_w), Some(150));
        assert_eq!(a.with(|s| s.cards[0].limit_w), Some(300), "the first backend is untouched");
        m.set_fan_pct(1, 70).unwrap();
        assert!(a.with(|s| s.fan_writes.contains(&(1, 1, 70))));
        assert_eq!(m.temp_c(0).unwrap(), 40);
        assert!(m.driver().contains(" + "));
    }
}
