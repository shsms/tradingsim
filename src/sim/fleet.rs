//! Per-contract counterparty lifecycle.
//!
//! A fleet declares a recipe — band tables, scalar knobs, an area —
//! and the FleetManager turns that into one MM (or N×P aggressors)
//! per delivery contract in a rolling window. As contracts age
//! through the window the per-contract task re-reads its fleet's
//! params each refresh and applies the band lookup, so size /
//! half-spread / fire-rate tracks the contract's current offset to
//! gate. When a contract gates, FleetManager signals its tasks to
//! cancel their resting quotes and exit; one new contract is
//! spawned at the far edge so the window stays full.
//!
//! Contrast with the previous slot-indexed model where each MM
//! rotated its `period.start` every 15 min: the rotation carried
//! per-MM state (`reference_price`, RNG, `follow_last_trade`
//! residue) onto a different contract, producing a visible price
//! step at every quarter boundary because the new contract
//! inherited the previous contract's drift. Per-contract MMs are
//! born with a fixed period and their state evolves with the
//! contract they quote.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use rust_decimal::Decimal;
use tokio::sync::oneshot;

use crate::lisp::{AggressorFleetSpec, MmFleetSpec, next_quarter_boundary};
use crate::scenarios::{AggressorView, MmView, SharedCurve, effective_ref};
use crate::sim::clock::SharedClock;
use crate::sim::counterparty::{
    Aggressor, AggressorConfig, MarketMaker, MarketMakerConfig, SharedAggressorConfig,
    SharedAggressorFleetParams, SharedConfig, SharedFleetParams,
};
use crate::sim::market::{Area, Currency, DeliveryDuration, DeliveryPeriod};
use crate::sim::weather::SharedWeather;
use crate::sim::world::World;

/// Handle the FleetManager holds for one running per-contract task.
/// Dropping the sender retires the task on its next select pass; an
/// explicit `send(())` makes the retirement happen immediately. Both
/// paths still let the task cancel resting quotes before exit.
struct ContractHandle {
    retire_tx: oneshot::Sender<()>,
}

/// One MM fleet's state: the spec (recipe) plus the per-contract
/// retire signals keyed by the contract's delivery start.
struct MmFleetState {
    spec: MmFleetSpec,
    active: HashMap<DateTime<Utc>, ContractHandle>,
}

/// One aggressor fleet's state. Per-(contract, profile_index) keys —
/// each contract hosts one aggressor per profile in the recipe.
struct AggressorFleetState {
    spec: AggressorFleetSpec,
    active: HashMap<(DateTime<Utc>, usize), ContractHandle>,
}

/// Owns the per-contract counterparty lifecycle for every fleet in
/// the running sim. Wrap in `Arc<Mutex<>>` so the lifecycle task can
/// call `roll_forward` on each quarter boundary while `add_*_fleet`
/// is also reachable.
pub struct FleetManager {
    world: Arc<RwLock<World>>,
    curve: SharedCurve,
    weather: SharedWeather,
    clock: SharedClock,
    mm_views: Arc<Mutex<Vec<MmView>>>,
    aggressor_views: Arc<Mutex<Vec<AggressorView>>>,
    mm_fleets: Vec<MmFleetState>,
    aggressor_fleets: Vec<AggressorFleetState>,
}

impl FleetManager {
    pub fn new(
        world: Arc<RwLock<World>>,
        curve: SharedCurve,
        weather: SharedWeather,
        clock: SharedClock,
    ) -> Self {
        Self {
            world,
            curve,
            weather,
            clock,
            mm_views: Arc::new(Mutex::new(Vec::new())),
            aggressor_views: Arc::new(Mutex::new(Vec::new())),
            mm_fleets: Vec::new(),
            aggressor_fleets: Vec::new(),
        }
    }

    /// Snapshot of every running MM, in registration order. The bias
    /// tick locks this each cycle to read demand/surplus targets +
    /// the reference baseline; contention is rare since the manager
    /// only mutates at startup + each quarter rotation.
    pub fn mm_views(&self) -> Arc<Mutex<Vec<MmView>>> {
        self.mm_views.clone()
    }

    pub fn aggressor_views(&self) -> Arc<Mutex<Vec<AggressorView>>> {
        self.aggressor_views.clone()
    }

    /// Register a fleet + spawn one MM per contract in the current
    /// window. Subsequent quarter rotations roll the window forward.
    pub fn add_mm_fleet(&mut self, spec: MmFleetSpec) {
        let now = Utc::now();
        let base = next_quarter_boundary(now);
        let mut active = HashMap::new();
        for i in 0..spec.window_quarters {
            let period_start = base + chrono::Duration::minutes(15 * i);
            let handle = spawn_mm_contract(
                self.world.clone(),
                &spec,
                self.curve.clone(),
                self.weather.clone(),
                self.clock.clone(),
                self.mm_views.clone(),
                period_start,
                i,
            );
            active.insert(period_start, handle);
        }
        self.mm_fleets.push(MmFleetState { spec, active });
    }

    pub fn add_aggressor_fleet(&mut self, spec: AggressorFleetSpec) {
        let now = Utc::now();
        let base = next_quarter_boundary(now);
        let profile_count = spec.shared_params.read().profile_sizes.len();
        let mut active = HashMap::new();
        for i in 0..spec.window_quarters {
            let period_start = base + chrono::Duration::minutes(15 * i);
            for p in 0..profile_count {
                let handle = spawn_aggressor_contract(
                    self.world.clone(),
                    &spec,
                    self.curve.clone(),
                    self.weather.clone(),
                    self.clock.clone(),
                    self.aggressor_views.clone(),
                    period_start,
                    i,
                    p,
                );
                active.insert((period_start, p), handle);
            }
        }
        self.aggressor_fleets.push(AggressorFleetState { spec, active });
    }

    /// Retire any per-contract task whose contract has gated and
    /// spawn fresh ones for contracts that have newly entered the
    /// far edge of the window. Called on each quarter boundary by
    /// the lifecycle task.
    pub fn roll_forward(&mut self, now: DateTime<Utc>) {
        let base = next_quarter_boundary(now);
        // MMs: retire gated then top up the window.
        for fleet_state in &mut self.mm_fleets {
            let to_retire: Vec<_> = fleet_state
                .active
                .keys()
                .filter(|p| **p < base)
                .cloned()
                .collect();
            for period_start in to_retire {
                if let Some(h) = fleet_state.active.remove(&period_start) {
                    let _ = h.retire_tx.send(());
                }
            }
            for i in 0..fleet_state.spec.window_quarters {
                let period_start = base + chrono::Duration::minutes(15 * i);
                if !fleet_state.active.contains_key(&period_start) {
                    let handle = spawn_mm_contract(
                        self.world.clone(),
                        &fleet_state.spec,
                        self.curve.clone(),
                        self.weather.clone(),
                        self.clock.clone(),
                        self.mm_views.clone(),
                        period_start,
                        i,
                    );
                    fleet_state.active.insert(period_start, handle);
                }
            }
        }
        // Aggressors: same shape, keyed by (period, profile).
        for fleet_state in &mut self.aggressor_fleets {
            let to_retire: Vec<_> = fleet_state
                .active
                .keys()
                .filter(|(p, _)| *p < base)
                .cloned()
                .collect();
            for key in to_retire {
                if let Some(h) = fleet_state.active.remove(&key) {
                    let _ = h.retire_tx.send(());
                }
            }
            let profile_count = fleet_state.spec.shared_params.read().profile_sizes.len();
            for i in 0..fleet_state.spec.window_quarters {
                let period_start = base + chrono::Duration::minutes(15 * i);
                for p in 0..profile_count {
                    if !fleet_state.active.contains_key(&(period_start, p)) {
                        let handle = spawn_aggressor_contract(
                            self.world.clone(),
                            &fleet_state.spec,
                            self.curve.clone(),
                            self.weather.clone(),
                            self.clock.clone(),
                            self.aggressor_views.clone(),
                            period_start,
                            i,
                            p,
                        );
                        fleet_state.active.insert((period_start, p), handle);
                    }
                }
            }
        }
    }

    /// Spawn a tokio task that calls `roll_forward` on each upcoming
    /// quarter boundary. Sleeps the exact time-to-boundary so the
    /// rotation happens at the same instant the gating contract's
    /// matcher would reject further submissions, not 100ms later.
    pub fn start_lifecycle_task(manager: Arc<Mutex<Self>>) {
        tokio::spawn(async move {
            loop {
                let now = Utc::now();
                let next = next_quarter_boundary(now);
                let wait = (next - now)
                    .to_std()
                    .unwrap_or_else(|_| Duration::from_secs(1));
                tokio::time::sleep(wait).await;
                manager.lock().roll_forward(Utc::now());
            }
        });
    }
}

/// Average `effective_ref` across `area_codes`. A fleet spanning the
/// four DE control zones gets a national reference instead of locking
/// to any single TSO's weather — matches Germany's single-bidding-zone
/// reality. Single-area fleets short-circuit to the same value
/// `effective_ref` would return alone.
fn fundamentals_at(
    curve: &SharedCurve,
    weather: &SharedWeather,
    clock: &SharedClock,
    area_codes: &[String],
    period_start: DateTime<Utc>,
) -> Decimal {
    debug_assert!(!area_codes.is_empty(), "fundamentals_at needs at least one area");
    let curve = curve.read();
    let weather = weather.read();
    let clock = clock.read().clone();
    let hour = clock.local_hour(period_start);
    let day = clock.local_day_of_year(period_start);
    let mut sum = Decimal::ZERO;
    for code in area_codes {
        sum += effective_ref(&curve, weather.for_area(code), hour, day);
    }
    sum / Decimal::from(area_codes.len() as i64)
}

/// Build a per-contract MM, register its view, and spawn the refresh
/// task. Returns a [`ContractHandle`] whose `retire_tx` causes the
/// task to cancel its resting quotes and exit.
fn spawn_mm_contract(
    world: Arc<RwLock<World>>,
    spec: &MmFleetSpec,
    curve: SharedCurve,
    weather: SharedWeather,
    clock: SharedClock,
    mm_views: Arc<Mutex<Vec<MmView>>>,
    period_start: DateTime<Utc>,
    initial_offset: i64,
) -> ContractHandle {
    let period = DeliveryPeriod {
        start: period_start,
        duration: DeliveryDuration::DeliveryDuration15,
    };
    let areas: Vec<Area> = spec.areas.iter().map(|c| Area::eic(c)).collect();
    let seeded = fundamentals_at(&curve, &weather, &clock, &spec.areas, period_start);
    let params = spec.shared_params.read().clone();
    let cfg = MarketMakerConfig {
        areas,
        period,
        currency: Currency::Eur,
        reference_baseline: seeded,
        reference_price: seeded,
        spread: params.spread_for(initial_offset, spec.window_quarters),
        size: params.size_for(initial_offset, spec.window_quarters),
        demand: Decimal::ZERO,
        surplus: Decimal::ZERO,
        price_noise: params.price_noise,
        tick: params.tick,
        follow_last_trade: params.follow_last_trade,
        refresh_ms: params.refresh_ms,
    };
    let shared_config = Arc::new(RwLock::new(cfg));
    let mm = MarketMaker::with_shared_config(
        shared_config.clone(),
        spec.seed_base.wrapping_add(initial_offset as u64),
    );
    mm_views.lock().push(MmView {
        shared_config: shared_config.clone(),
    });
    let (retire_tx, retire_rx) = oneshot::channel();
    tokio::spawn(run_mm_contract(
        world,
        mm,
        spec.shared_params.clone(),
        spec.window_quarters,
        shared_config,
        mm_views,
        retire_rx,
    ));
    ContractHandle { retire_tx }
}

async fn run_mm_contract(
    world: Arc<RwLock<World>>,
    mut mm: MarketMaker,
    fleet_params: SharedFleetParams,
    window_quarters: i64,
    shared_config: SharedConfig,
    mm_views: Arc<Mutex<Vec<MmView>>>,
    mut retire_rx: oneshot::Receiver<()>,
) {
    loop {
        let now = Utc::now();
        // Re-stamp band-effective size + spread + scalars before the
        // quote step reads them. The contract's offset to the next
        // boundary shrinks as the MM ages, so the band index
        // gradually steps front-ward — front-band depth + tight
        // spread arrive naturally near gate.
        let wait_ms = {
            let mut c = shared_config.write();
            let p = fleet_params.read();
            let base = next_quarter_boundary(now);
            let offset = ((c.period.start - base).num_minutes() / 15).max(0);
            c.size = p.size_for(offset, window_quarters);
            c.spread = p.spread_for(offset, window_quarters);
            c.price_noise = p.price_noise;
            c.follow_last_trade = p.follow_last_trade;
            c.tick = p.tick;
            c.refresh_ms = p.refresh_ms;
            c.refresh_ms
        };
        {
            let mut w = world.write();
            mm.refresh(&mut w, now);
        }
        tokio::select! {
            _ = &mut retire_rx => {
                let mut w = world.write();
                mm.cancel_resting(&mut w);
                drop(w);
                mm_views
                    .lock()
                    .retain(|v| !Arc::ptr_eq(&v.shared_config, &shared_config));
                return;
            }
            _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => {}
        }
    }
}

fn spawn_aggressor_contract(
    world: Arc<RwLock<World>>,
    spec: &AggressorFleetSpec,
    curve: SharedCurve,
    weather: SharedWeather,
    clock: SharedClock,
    aggressor_views: Arc<Mutex<Vec<AggressorView>>>,
    period_start: DateTime<Utc>,
    initial_offset: i64,
    profile: usize,
) -> ContractHandle {
    let period = DeliveryPeriod {
        start: period_start,
        duration: DeliveryDuration::DeliveryDuration15,
    };
    let area = Area::eic(&spec.area);
    let seeded = fundamentals_at(
        &curve,
        &weather,
        &clock,
        std::slice::from_ref(&spec.area),
        period_start,
    );
    let params = spec.shared_params.read().clone();
    let cfg = AggressorConfig {
        area: area.clone(),
        period,
        currency: Currency::Eur,
        size: params.size_for(profile),
        side_bias: params.side_bias,
        reference_price: seeded,
        rate_ms: params.rate_ms_for(profile, initial_offset),
        max_slippage: params.max_slippage,
    };
    let shared_config = Arc::new(RwLock::new(cfg));
    let seed = spec
        .seed_base
        .wrapping_add((initial_offset as u64) * 100)
        .wrapping_add(profile as u64);
    let ag = Aggressor::with_shared_config(shared_config.clone(), seed);
    aggressor_views.lock().push(AggressorView {
        shared_config: shared_config.clone(),
    });
    let (retire_tx, retire_rx) = oneshot::channel();
    tokio::spawn(run_aggressor_contract(
        world,
        ag,
        spec.shared_params.clone(),
        profile,
        shared_config,
        aggressor_views,
        retire_rx,
    ));
    ContractHandle { retire_tx }
}

async fn run_aggressor_contract(
    world: Arc<RwLock<World>>,
    mut ag: Aggressor,
    fleet_params: SharedAggressorFleetParams,
    profile: usize,
    shared_config: SharedAggressorConfig,
    aggressor_views: Arc<Mutex<Vec<AggressorView>>>,
    mut retire_rx: oneshot::Receiver<()>,
) {
    loop {
        let now = Utc::now();
        let base_secs = {
            let mut c = shared_config.write();
            let p = fleet_params.read();
            let base = next_quarter_boundary(now);
            let offset = ((c.period.start - base).num_minutes() / 15).max(0);
            c.size = p.size_for(profile);
            c.max_slippage = p.max_slippage;
            c.rate_ms = p.rate_ms_for(profile, offset);
            c.rate_ms as f64 / 1000.0
        };
        let scale =
            crate::sim::counterparty::gate_close_scale(shared_config.read().period.start, now);
        let wait = Duration::from_secs_f64((base_secs / scale.max(1.0)).max(0.05));
        tokio::select! {
            _ = &mut retire_rx => {
                aggressor_views
                    .lock()
                    .retain(|v| !Arc::ptr_eq(&v.shared_config, &shared_config));
                return;
            }
            _ = tokio::time::sleep(wait) => {}
        }
        let mut w = world.write();
        ag.fire(&mut w, Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lisp::Config as LispConfig;
    use crate::sim::market::{MarketRegistry, MarketRules};

    fn empty_world() -> Arc<RwLock<World>> {
        let mut markets = MarketRegistry::new();
        markets.insert(MarketRules::default_for_tests());
        Arc::new(RwLock::new(World::new(markets)))
    }

    /// Lifecycle plumbing: adding a fleet immediately publishes one
    /// MmView per contract in the window, all sharing the fleet's
    /// SharedFleetParams Arc.
    #[tokio::test]
    async fn add_mm_fleet_publishes_window_views() {
        let cfg = LispConfig::with_defaults();
        let world = empty_world();
        let mut mgr = FleetManager::new(
            world,
            cfg.curve(),
            cfg.weather(),
            cfg.clock(),
        );
        let spec = MmFleetSpec {
            name: "test".into(),
            areas: vec!["10YDE-EON------1".into()],
            window_quarters: 4,
            shared_params: Arc::new(RwLock::new(
                crate::sim::counterparty::MmFleetParams::default(),
            )),
            seed_base: 0,
        };
        mgr.add_mm_fleet(spec);
        let views = mgr.mm_views();
        let v = views.lock();
        assert_eq!(v.len(), 4);
        // Each view sits on a distinct contract.
        let mut starts: Vec<_> = v
            .iter()
            .map(|x| x.shared_config.read().period.start)
            .collect();
        starts.sort();
        starts.dedup();
        assert_eq!(starts.len(), 4);
    }

    #[tokio::test]
    async fn roll_forward_retires_gated_and_tops_up() {
        let cfg = LispConfig::with_defaults();
        let world = empty_world();
        let mut mgr = FleetManager::new(
            world,
            cfg.curve(),
            cfg.weather(),
            cfg.clock(),
        );
        let spec = MmFleetSpec {
            name: "test".into(),
            areas: vec!["10YDE-EON------1".into()],
            window_quarters: 2,
            shared_params: Arc::new(RwLock::new(
                crate::sim::counterparty::MmFleetParams::default(),
            )),
            seed_base: 0,
        };
        mgr.add_mm_fleet(spec);
        // Pretend 15 minutes have passed — the front contract just
        // gated. roll_forward should retire one + spawn one fresh
        // at the far edge, keeping the window at 2.
        let later = Utc::now() + chrono::Duration::minutes(15);
        mgr.roll_forward(later);
        // Give the retired task a tick to drop its view entry.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let views = mgr.mm_views();
        let v = views.lock();
        assert_eq!(v.len(), 2, "window should stay full at 2");
    }
}
