//! Header pulse bar: status pills, per-area sparkbars over a rolling
//! 60 s window, density toggle, and a once-per-second clock. The
//! scenario indicator + sim-tz-aware clock land in follow-ups once
//! the scenarios signal lifts into context and an Intl helper exists.

use std::collections::HashMap;

use gloo_timers::future::TimeoutFuture;
use js_sys::{Date, Object, Reflect};
use leptos::prelude::*;
use wasm_bindgen::JsValue;

use crate::util::{ALL_AREAS, AreaGroup};

const BUCKETS: usize = 12;
const BUCKET_MS: u32 = 5_000;
const DENSITY_KEY: &str = "tradingsim-density";

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

fn format_clock_now() -> String {
    let d = Date::new_0();
    let opts = Object::new();
    let _ = Reflect::set(&opts, &"hour".into(), &"2-digit".into());
    let _ = Reflect::set(&opts, &"minute".into(), &"2-digit".into());
    let _ = Reflect::set(&opts, &"second".into(), &"2-digit".into());
    let _ = Reflect::set(&opts, &"hour12".into(), &JsValue::FALSE);
    String::from(d.to_locale_time_string_with_options("en-GB", &opts))
}

#[component]
pub fn PulseBar() -> impl IntoView {
    let trade_count = expect_context::<ReadSignal<usize>>();
    let weather_loaded = expect_context::<RwSignal<bool>>();
    let spark = expect_context::<RwSignal<SparkState>>();

    // Sparkbar rotation tick — empties the oldest bucket every
    // BUCKET_MS so a quiet 60 s decays a previously busy area to
    // zero bars.
    leptos::task::spawn_local(async move {
        loop {
            TimeoutFuture::new(BUCKET_MS).await;
            spark.update(|s| s.rotate());
        }
    });

    // Wallclock tick. Browser-local for now — sim-tz formatting
    // shares the Intl helper with the trades / scenarios panels
    // once that lands.
    let (clock, set_clock) = signal(format_clock_now());
    leptos::task::spawn_local(async move {
        loop {
            set_clock.set(format_clock_now());
            TimeoutFuture::new(1000).await;
        }
    });

    // Density toggle: hydrate from localStorage, then mirror to
    // the body class on every change.
    let (comfortable, set_comfortable) = signal(initial_density());
    apply_density(comfortable.get_untracked());
    Effect::new(move |_| {
        let c = comfortable.get();
        apply_density(c);
        save_density(c);
    });
    let toggle_density = move |_| set_comfortable.update(|c| *c = !*c);

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

    let density_label = move || if comfortable.get() { "comfortable" } else { "compact" };

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
            <div class="pulse-group pulse-right">
                <span class="chip" on:click=toggle_density>{density_label}</span>
                <span class="pulse-clock">{clock}</span>
            </div>
        </section>
    }
}
