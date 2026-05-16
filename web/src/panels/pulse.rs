//! Header pulse bar: status pills, per-home-area sparkbars over a
//! rolling 60 s window, scenario indicator, density + tz toggles,
//! and a once-per-second sim-tz clock.

use std::collections::HashMap;

use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;

use crate::intl::{now_hms, zone_label};
use crate::types::Scenario;
use crate::util::{ALL_AREAS, AreaGroup};

const BUCKETS: usize = 12;
const BUCKET_MS: u32 = 5_000;
const DENSITY_KEY: &str = "tradingsim-density";
const TZ_KEY: &str = "tradingsim-tz";

/// User's tz display preference. Local means "format in the sim's
/// home zone (Europe/Berlin in the shipped config)"; Utc means
/// "format in UTC regardless of the sim's home zone".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TzMode {
    Local,
    Utc,
}

#[derive(Debug, Clone)]
pub struct SparkState {
    /// Per-home-area rolling counts. Newest bucket is the last slot;
    /// the rotate loop shifts the oldest off and pushes a fresh zero.
    pub buckets: HashMap<&'static str, [u32; BUCKETS]>,
}

impl Default for SparkState {
    fn default() -> Self {
        let buckets = ALL_AREAS
            .iter()
            .filter(|a| a.group == AreaGroup::Home)
            .map(|a| (a.code, [0u32; BUCKETS]))
            .collect();
        Self { buckets }
    }
}

impl SparkState {
    pub fn record(&mut self, area: &str) {
        if let Some(b) = self.buckets.get_mut(area) {
            let last = BUCKETS - 1;
            b[last] = b[last].saturating_add(1);
        }
    }

    fn rotate(&mut self) {
        for b in self.buckets.values_mut() {
            for i in 0..BUCKETS - 1 {
                b[i] = b[i + 1];
            }
            b[BUCKETS - 1] = 0;
        }
    }
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn document_body() -> Option<web_sys::HtmlElement> {
    web_sys::window()?.document()?.body()
}

fn apply_density(comfortable: bool) {
    if let Some(body) = document_body() {
        let _ = body.class_list().toggle_with_force("comfortable", comfortable);
    }
}

fn initial_density() -> bool {
    if let Some(ls) = local_storage()
        && let Ok(Some(raw)) = ls.get_item(DENSITY_KEY)
    {
        return raw == "comfortable";
    }
    // First-load default — comfortable on wide displays, compact on
    // narrow ones. Matches the JS UI's breakpoint.
    web_sys::window()
        .and_then(|w| w.inner_width().ok())
        .and_then(|v| v.as_f64())
        .map(|w| w >= 1800.0)
        .unwrap_or(false)
}

fn save_density(comfortable: bool) {
    if let Some(ls) = local_storage() {
        let _ = ls.set_item(
            DENSITY_KEY,
            if comfortable { "comfortable" } else { "compact" },
        );
    }
}

pub fn load_tz_mode() -> TzMode {
    match local_storage().and_then(|ls| ls.get_item(TZ_KEY).ok().flatten()) {
        Some(s) if s == "utc" => TzMode::Utc,
        _ => TzMode::Local,
    }
}

fn save_tz_mode(mode: TzMode) {
    if let Some(ls) = local_storage() {
        let v = match mode {
            TzMode::Local => "local",
            TzMode::Utc => "utc",
        };
        let _ = ls.set_item(TZ_KEY, v);
    }
}

#[component]
pub fn PulseBar() -> impl IntoView {
    let trade_count = expect_context::<ReadSignal<usize>>();
    let weather_loaded = expect_context::<RwSignal<bool>>();
    let spark = expect_context::<RwSignal<SparkState>>();
    let display_tz = expect_context::<RwSignal<String>>();
    let tz_mode = expect_context::<RwSignal<TzMode>>();
    let scenarios = expect_context::<RwSignal<Vec<Scenario>>>();

    // Sparkbar rotation tick — empties the oldest bucket every
    // BUCKET_MS so a quiet 60 s decays a previously busy area to
    // zero bars.
    leptos::task::spawn_local(async move {
        loop {
            TimeoutFuture::new(BUCKET_MS).await;
            spark.update(|s| s.rotate());
        }
    });

    // Wallclock tick. The tick signal toggles each second so the
    // formatter re-runs; the zone label suffix (CEST / CET / UTC)
    // changes with the tz toggle without separate plumbing.
    let (tick, set_tick) = signal(0_u64);
    leptos::task::spawn_local(async move {
        loop {
            TimeoutFuture::new(1000).await;
            set_tick.update(|n| *n = n.wrapping_add(1));
        }
    });
    let clock_view = move || {
        let _ = tick.get();
        let tz = display_tz.get();
        format!("{} {}", now_hms(&tz), zone_label(&tz))
    };

    // Density toggle.
    let (comfortable, set_comfortable) = signal(initial_density());
    apply_density(comfortable.get_untracked());
    Effect::new(move |_| {
        let c = comfortable.get();
        apply_density(c);
        save_density(c);
    });
    let toggle_density = move |_| set_comfortable.update(|c| *c = !*c);
    let density_label = move || if comfortable.get() { "comfortable" } else { "compact" };

    // TZ toggle — flips between sim local and UTC; clock + every
    // panel that formats through `display_tz` follow immediately.
    let toggle_tz = move |_| {
        tz_mode.update(|m| {
            *m = match m {
                TzMode::Local => TzMode::Utc,
                TzMode::Utc => TzMode::Local,
            };
            save_tz_mode(*m);
        });
    };
    let tz_label = move || match tz_mode.get() {
        TzMode::Local => "local",
        TzMode::Utc => "UTC",
    };

    let pill_trades_cls = move || {
        if trade_count.get() > 0 { "pill ok" } else { "pill down" }
    };
    let pill_weather_cls = move || {
        if weather_loaded.get() { "pill ok" } else { "pill down" }
    };

    let sparkbars = move || {
        let s = spark.get();
        ALL_AREAS
            .iter()
            .filter(|a| a.group == AreaGroup::Home)
            .map(|a| {
                let bs = s.buckets.get(a.code).copied().unwrap_or([0; BUCKETS]);
                let max = bs.iter().copied().max().unwrap_or(1).max(1);
                let bars = bs
                    .iter()
                    .map(|n| {
                        let h = ((*n as f64 / max as f64) * 14.0).round().max(1.0) as i64;
                        view! { <span class="spark-bar" style=format!("height:{h}px")></span> }
                    })
                    .collect_view();
                view! {
                    <span class="spark-item">
                        <span class="area-badge">{a.tag}</span>
                        <span class="spark">{bars}</span>
                    </span>
                }
            })
            .collect_view()
    };

    let scenario_indicator = move || {
        let list = scenarios.get();
        let active = list.iter().find(|s| s.current_stage.is_some());
        match active {
            Some(s) => {
                let idx = s.current_stage.unwrap_or(0);
                let stage_name = s
                    .stages
                    .get(idx)
                    .map(|st| st.name.as_str())
                    .unwrap_or("?");
                let total = s.stages.len();
                (
                    "".to_string(),
                    format!("{} · {} ({}/{})", s.name, stage_name, idx + 1, total),
                )
            }
            None => ("muted".to_string(), "—".to_string()),
        }
    };
    let indicator_cls = move || scenario_indicator().0;
    let indicator_text = move || scenario_indicator().1;

    view! {
        <section class="pulse" aria-label="system pulse">
            <div class="pulse-group">
                <span class=pill_trades_cls>"trades"</span>
                <span class=pill_weather_cls>"weather"</span>
            </div>
            <div class="pulse-sep"></div>
            <div class="pulse-group" id="spark-row" title="prints per 5s, last 60s">
                {sparkbars}
            </div>
            <div class="pulse-sep"></div>
            <div class="pulse-group">
                <span class="muted">"scenario"</span>
                <span class=indicator_cls>{indicator_text}</span>
            </div>
            <div class="pulse-group pulse-right">
                <span class="chip" on:click=toggle_tz>{tz_label}</span>
                <span class="chip" on:click=toggle_density>{density_label}</span>
                <span class="pulse-clock">{clock_view}</span>
            </div>
        </section>
    }
}
