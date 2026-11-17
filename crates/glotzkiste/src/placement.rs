//! Which GPU decodes a stream (owner, 2026-11-16): the other GPUs first, in
//! Windows' adapter order, the main GPU (the one drawing the window, also
//! busy with the browser, games and the desktop) last, the CPU only when
//! every video decoder is full.
//!
//! The decoder load comes from the GPU monitor every 2 s. Between samples,
//! streams placed since the last one count with an estimated cost, so a
//! restored session of twenty streams spreads out instead of piling onto
//! the first GPU.

use crate::gpu::Gpu;
use crate::stream::DecodeOn;
use std::collections::{HashMap, HashSet};

/// A decoder fuller than this takes no new stream.
const ROOM_PCT: f64 = 80.0;
/// Busy (Auto quality drops the 60 fps preference) from here ...
const BUSY_ON_PCT: f64 = 85.0;
/// ... until it falls below this.
const BUSY_OFF_PCT: f64 = 60.0;
/// Decoder share of one stream before anything was measured.
const DEFAULT_COST_PCT: f64 = 20.0;

type Luid = (u32, i32);

#[derive(Default)]
pub struct Placer {
    main: Luid,
    gpus: Vec<Gpu>,
    /// No hardware decoding there (a stream fell back to software).
    bad: HashSet<Luid>,
    /// Streams placed since the last load sample.
    placed: HashMap<Luid, u32>,
    /// Streams decoding on each GPU at the last sample.
    streams_on: HashMap<Luid, u32>,
    busy: HashSet<Luid>,
    /// The viewer turned the other GPUs off (settings): main GPU, then CPU.
    others_off: bool,
}

impl Placer {
    /// Use the other GPUs (default) or only the main one, for streams placed
    /// from now on.
    pub fn set_others(&mut self, on: bool) {
        self.others_off = !on;
    }

    pub fn new(main: Luid, gpus: Vec<Gpu>) -> Self {
        Self { main, gpus, ..Default::default() }
    }

    /// The GPUs in placement order: the others, then the main one.
    pub fn order(&self) -> Vec<&Gpu> {
        let mut v: Vec<&Gpu> = self
            .gpus
            .iter()
            .filter(|g| g.luid != self.main && !self.bad.contains(&g.luid) && !self.others_off)
            .collect();
        v.extend(self.gpus.iter().filter(|g| g.luid == self.main));
        v
    }

    fn cost(&self, g: &Gpu) -> f64 {
        match self.streams_on.get(&g.luid) {
            Some(&n) if n > 0 && g.decode_pct > 0.0 => (g.decode_pct / f64::from(n)).clamp(3.0, 50.0),
            _ => DEFAULT_COST_PCT,
        }
    }

    fn has_room(&self, g: &Gpu) -> bool {
        let cost = self.cost(g);
        let pending = f64::from(self.placed.get(&g.luid).copied().unwrap_or(0)) * cost;
        !self.busy.contains(&g.luid) && g.decode_pct + pending + cost <= ROOM_PCT
    }

    fn on(&self, luid: Luid) -> DecodeOn {
        if luid == self.main { DecodeOn::Gpu } else { DecodeOn::Second(luid) }
    }

    /// Where a new stream goes: the first GPU in order with room. With none
    /// left, the main GPU (Auto quality steps down there, and the valve
    /// moves streams on if frames drop).
    pub fn choose(&mut self) -> DecodeOn {
        self.choose_except(None).unwrap_or_else(|| {
            *self.placed.entry(self.main).or_default() += 1;
            DecodeOn::Gpu
        })
    }

    /// Like `choose`, skipping `except`; `None` when no GPU has room.
    pub fn choose_except(&mut self, except: Option<Luid>) -> Option<DecodeOn> {
        let l =
            self.order().into_iter().filter(|g| Some(g.luid) != except).find(|g| self.has_room(g)).map(|g| g.luid)?;
        *self.placed.entry(l).or_default() += 1;
        Some(self.on(l))
    }

    /// A GPU earlier in placement order than `from` (so: not the main GPU
    /// when `from` is another one) with room for one more stream. Used to
    /// move streams back toward the preferred GPUs.
    pub fn earlier_with_room(&mut self, from: Luid) -> Option<DecodeOn> {
        let order: Vec<Luid> = self.order().iter().map(|g| g.luid).collect();
        let pos = order.iter().position(|l| *l == from)?;
        let l = order[..pos]
            .iter()
            .copied()
            .find(|l| self.gpus.iter().find(|g| g.luid == *l).is_some_and(|g| self.has_room(g)))?;
        *self.placed.entry(l).or_default() += 1;
        Some(self.on(l))
    }

    /// Position in placement order (0 first); the CPU after every GPU.
    pub fn rank(&self, on: DecodeOn) -> usize {
        let luid = match on {
            DecodeOn::Gpu => self.main,
            DecodeOn::Second(l) => l,
            DecodeOn::Cpu => return usize::MAX,
        };
        self.order().iter().position(|g| g.luid == luid).unwrap_or(usize::MAX - 1)
    }

    /// A new load sample: `streams_on` counts the streams decoding on each
    /// GPU now.
    pub fn update(&mut self, gpus: Vec<Gpu>, streams_on: HashMap<Luid, u32>) {
        if gpus.is_empty() {
            return;
        }
        for g in &gpus {
            if !self.busy.contains(&g.luid) && g.decode_pct >= BUSY_ON_PCT {
                log::warn!(
                    "{}: video decoder at {:.0} %, Auto quality sizes by tile height there",
                    g.name,
                    g.decode_pct
                );
                self.busy.insert(g.luid);
            } else if g.decode_pct < BUSY_OFF_PCT {
                self.busy.remove(&g.luid);
            }
        }
        self.gpus = gpus;
        self.streams_on = streams_on;
        self.placed.clear();
    }

    /// A stream on this GPU decoded in software: it has no usable hardware
    /// decoder for these streams; place nothing there any more.
    pub fn mark_bad(&mut self, luid: Luid) {
        if luid != self.main && self.bad.insert(luid) {
            log::warn!("{}: no hardware video decoding there, not used for streams", self.name(DecodeOn::Second(luid)));
        }
    }

    /// The decoder of `on` is busy (CPU decoding always counts as busy).
    pub fn busy(&self, on: DecodeOn) -> bool {
        match on {
            DecodeOn::Gpu => self.busy.contains(&self.main),
            DecodeOn::Second(l) => self.busy.contains(&l),
            DecodeOn::Cpu => true,
        }
    }

    /// A short name for where a stream decodes ("Radeon RX 9070 XT").
    pub fn name(&self, on: DecodeOn) -> String {
        let luid = match on {
            DecodeOn::Gpu => self.main,
            DecodeOn::Second(l) => l,
            DecodeOn::Cpu => return "CPU".into(),
        };
        self.gpus.iter().find(|g| g.luid == luid).map(|g| short_name(&g.name)).unwrap_or_else(|| "GPU".into())
    }

    /// The choices for a tile's menu, in placement order: (where, label).
    pub fn menu(&self) -> Vec<(DecodeOn, String)> {
        let mut v: Vec<(DecodeOn, String)> = self
            .gpus
            .iter()
            .filter(|g| g.luid != self.main)
            .map(|g| (DecodeOn::Second(g.luid), short_name(&g.name)))
            .collect();
        v.push((DecodeOn::Gpu, format!("{} (main)", self.name(DecodeOn::Gpu))));
        v.push((DecodeOn::Cpu, "CPU".into()));
        v
    }
}

/// "AMD Radeon(TM) Graphics" -> "Radeon Graphics".
pub fn short_name(name: &str) -> String {
    let mut s = name.replace("(TM)", "").replace("(R)", "").replace("(tm)", "");
    for p in ["AMD ", "NVIDIA ", "Intel ", "GeForce "] {
        if let Some(rest) = s.strip_prefix(p) {
            s = rest.to_owned();
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(name: &str, luid: Luid, pct: f64) -> Gpu {
        Gpu { name: name.into(), luid, vendor: 0, decode_pct: pct }
    }

    const MAIN: Luid = (1, 0);
    const IGPU: Luid = (2, 0);
    const THIRD: Luid = (3, 0);

    #[test]
    fn other_gpus_first_main_last() {
        let mut p =
            Placer::new(MAIN, vec![gpu("AMD Radeon RX 9070 XT", MAIN, 0.0), gpu("AMD Radeon(TM) Graphics", IGPU, 0.0)]);
        // Default cost 20 %, room up to 80 %: four streams on the iGPU ...
        for _ in 0..4 {
            assert_eq!(p.choose(), DecodeOn::Second(IGPU));
        }
        // ... then the main GPU.
        assert_eq!(p.choose(), DecodeOn::Gpu);
    }

    #[test]
    fn measured_load_decides() {
        let mut p = Placer::new(MAIN, vec![gpu("main", MAIN, 0.0), gpu("igpu", IGPU, 0.0), gpu("third", THIRD, 0.0)]);
        p.update(
            vec![gpu("main", MAIN, 10.0), gpu("igpu", IGPU, 72.0), gpu("third", THIRD, 0.0)],
            HashMap::from([(IGPU, 3)]),
        );
        // 72 % + 24 % per stream does not fit: the third GPU, then main.
        assert_eq!(p.choose(), DecodeOn::Second(THIRD));
        assert_eq!(p.order().first().map(|g| g.luid), Some(IGPU));
        assert_eq!(p.order().last().map(|g| g.luid), Some(MAIN));
    }

    #[test]
    fn busy_has_hysteresis_and_bad_gpus_are_skipped() {
        let mut p = Placer::new(MAIN, vec![gpu("main", MAIN, 0.0), gpu("igpu", IGPU, 0.0)]);
        p.update(vec![gpu("main", MAIN, 0.0), gpu("igpu", IGPU, 90.0)], HashMap::from([(IGPU, 4)]));
        assert!(p.busy(DecodeOn::Second(IGPU)));
        p.update(vec![gpu("main", MAIN, 0.0), gpu("igpu", IGPU, 70.0)], HashMap::from([(IGPU, 4)]));
        assert!(p.busy(DecodeOn::Second(IGPU)), "stays busy above 60 %");
        p.update(vec![gpu("main", MAIN, 0.0), gpu("igpu", IGPU, 10.0)], HashMap::from([(IGPU, 1)]));
        assert!(!p.busy(DecodeOn::Second(IGPU)));
        p.mark_bad(IGPU);
        assert_eq!(p.choose(), DecodeOn::Gpu);
        assert!(p.busy(DecodeOn::Cpu));
    }

    #[test]
    fn moving_off_a_full_gpu() {
        let mut p = Placer::new(MAIN, vec![gpu("main", MAIN, 0.0), gpu("igpu", IGPU, 0.0)]);
        p.update(vec![gpu("main", MAIN, 95.0), gpu("igpu", IGPU, 95.0)], HashMap::from([(MAIN, 10), (IGPU, 4)]));
        assert_eq!(p.choose_except(Some(IGPU)), None, "nowhere to go: the caller uses the CPU");
        p.update(vec![gpu("main", MAIN, 30.0), gpu("igpu", IGPU, 95.0)], HashMap::from([(MAIN, 3), (IGPU, 4)]));
        assert_eq!(p.choose_except(Some(IGPU)), Some(DecodeOn::Gpu));
    }

    #[test]
    fn rebalancing_moves_toward_earlier_gpus_only() {
        let mut p = Placer::new(MAIN, vec![gpu("main", MAIN, 0.0), gpu("igpu", IGPU, 0.0)]);
        // The owner's 42 streams: main full, the iGPU at 30 % with 5 streams.
        p.update(vec![gpu("main", MAIN, 100.0), gpu("igpu", IGPU, 30.0)], HashMap::from([(MAIN, 37), (IGPU, 5)]));
        assert_eq!(p.earlier_with_room(MAIN), Some(DecodeOn::Second(IGPU)));
        // 6 % per stream there: room for several more before 80 %.
        let more = (0..20).filter(|_| p.earlier_with_room(MAIN).is_some()).count();
        assert!((6..=8).contains(&more), "{more}");
        // Nothing is earlier than the first GPU.
        assert_eq!(p.earlier_with_room(IGPU), None);
        assert_eq!(p.rank(DecodeOn::Second(IGPU)), 0);
        assert_eq!(p.rank(DecodeOn::Gpu), 1);
        assert_eq!(p.rank(DecodeOn::Cpu), usize::MAX);
    }

    #[test]
    fn short_names() {
        assert_eq!(short_name("AMD Radeon(TM) Graphics"), "Radeon Graphics");
        assert_eq!(short_name("NVIDIA GeForce RTX 5090"), "RTX 5090");
        assert_eq!(short_name("Intel(R) Arc(TM) A380 Graphics"), "Arc A380 Graphics");
    }
}
