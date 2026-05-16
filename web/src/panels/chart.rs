//! Price tape chart — canvas-based line over the last N minutes
//! of public-trade prints, bucketed + EMA-smoothed.

use std::collections::BTreeMap;

use gloo_timers::future::TimeoutFuture;
use js_sys::Date;
use leptos::html::Canvas;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::CanvasRenderingContext2d;

use crate::intl::short_time;
use crate::types::PublicTrade;
use crate::util::{ALL_AREAS, AreaGroup};

const PERIOD_STEP_MS: f64 = 15.0 * 60.0 * 1000.0;
const EMA_ALPHA: f64 = 0.4;
const LINE_COLOR: &str = "#58a6ff";
const BORDER_COLOR: &str = "#30363d";
const MUTED_COLOR: &str = "#8b949e";
const Y_STEPS: i32 = 5;

const KEY_WINDOW: &str = "tradingsim-chart-window-min";
const KEY_PERIOD: &str = "tradingsim-chart-period";

/// Window options the dropdown offers, in minutes. Matches the JS UI.
const WINDOW_OPTIONS: &[(u32, &str)] = &[
    (5, "5 min"),
    (10, "10 min"),
    (30, "30 min"),
    (60, "1 hour"),
    (240, "4 hours"),
    (720, "12 hours"),
    (1440, "24 hours"),
];

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn load_window() -> u32 {
    local_storage()
        .and_then(|ls| ls.get_item(KEY_WINDOW).ok().flatten())
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
}

fn save_window(mins: u32) {
    if let Some(ls) = local_storage() {
        let _ = ls.set_item(KEY_WINDOW, &mins.to_string());
    }
}

fn load_chart_period() -> Option<String> {
    local_storage().and_then(|ls| ls.get_item(KEY_PERIOD).ok().flatten())
}

fn save_chart_period(p: Option<&str>) {
    if let Some(ls) = local_storage() {
        match p {
            Some(v) => {
                let _ = ls.set_item(KEY_PERIOD, v);
            }
            None => {
                let _ = ls.remove_item(KEY_PERIOD);
            }
        }
    }
}

fn pick_x_step(window_ms: f64) -> f64 {
    const M: f64 = 60_000.0;
    for c in [1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0] {
        let step = c * M;
        if window_ms / step <= 8.0 {
            return step;
        }
    }
    window_ms
}

fn format_back(mins: i64) -> String {
    if mins == 0 {
        return "now".to_string();
    }
    if mins < 60 {
        return format!("-{mins}m");
    }
    let h = mins / 60;
    let m = mins % 60;
    if m == 0 {
        format!("-{h}h")
    } else {
        format!("-{h}h{m}m")
    }
}

fn home_areas() -> Vec<&'static str> {
    ALL_AREAS
        .iter()
        .filter(|a| a.group == AreaGroup::Home)
        .map(|a| a.code)
        .collect()
}

#[component]
pub fn PriceChart() -> impl IntoView {
    let trades = expect_context::<ReadSignal<Vec<PublicTrade>>>();
    let sim_tz = expect_context::<RwSignal<String>>();
    let window_min = RwSignal::new(load_window());
    let chart_period = RwSignal::new(load_chart_period());
    let canvas_ref = NodeRef::<Canvas>::new();

    // 1 s redraw tick; throttle vs the WS print rate (the resampler
    // averages within the bucket anyway, so per-print redraws would
    // just thrash the GPU).
    leptos::task::spawn_local(async move {
        loop {
            if let Some(canvas) = canvas_ref.get_untracked() {
                let win_ms = window_min.get_untracked() as f64 * 60_000.0;
                let pinned = chart_period.get_untracked();
                trades.with_untracked(|v| draw(&canvas, v, win_ms, pinned.as_deref()));
            }
            TimeoutFuture::new(1000).await;
        }
    });

    let on_window_change = move |ev| {
        if let Ok(n) = event_target_value(&ev).parse::<u32>() {
            window_min.set(n);
            save_window(n);
        }
    };

    let on_period_change = move |ev| {
        let v = event_target_value(&ev);
        let next = if v == "auto" || v.is_empty() { None } else { Some(v) };
        chart_period.set(next.clone());
        save_chart_period(next.as_deref());
    };

    let title = move || {
        let win_ms = window_min.get() as f64 * 60_000.0;
        let pinned = chart_period.get();
        let trades_snapshot = trades.get();
        let tz = sim_tz.get();
        let eff = effective_period_iso(&trades_snapshot, &home_areas(), win_ms, pinned.as_deref());
        let tag = if pinned.is_some() { "pinned" } else { "auto" };
        match eff {
            Some(iso) => format!("Price tape — delivery {} ({tag})", short_time(&iso, &tz)),
            None => "Price tape — delivery — (auto)".to_string(),
        }
    };

    let period_options = move || {
        let pinned = chart_period.get();
        let pinned_str = pinned.as_deref().unwrap_or("");
        let tz = sim_tz.get();
        let mut periods: Vec<String> =
            trades.with(|v| v.iter().map(|t| t.period.clone()).collect());
        periods.sort();
        periods.dedup();
        let mut opts: Vec<_> = vec![
            view! { <option value="auto" selected=pinned.is_none()>"auto (next contract)"</option> }
                .into_any(),
            view! { <option value="all">"all deliveries"</option> }.into_any(),
        ];
        for p in periods {
            let selected = p == pinned_str;
            let label = short_time(&p, &tz);
            opts.push(
                view! { <option value=p.clone() selected=selected>{label}</option> }.into_any(),
            );
        }
        opts.into_iter().collect_view()
    };

    view! {
        <section class="panel panel-chart">
            <div class="chart-head">
                <h2>{title}</h2>
                <div class="chart-controls">
                    <label>"window "
                        <select on:change=on_window_change>
                            {WINDOW_OPTIONS.iter().map(|(m, label)| {
                                let m = *m;
                                let selected = move || window_min.get() == m;
                                view! {
                                    <option value=m.to_string() selected=selected>{*label}</option>
                                }
                            }).collect_view()}
                        </select>
                    </label>
                    <label>"delivery "
                        <select on:change=on_period_change>{period_options}</select>
                    </label>
                </div>
            </div>
            <canvas node_ref=canvas_ref id="price-chart"></canvas>
        </section>
    }
}

/// Effective delivery period (epoch ms) the chart pins on, or None
/// if there's no data and no pinned choice. The pinned dropdown
/// choice wins; otherwise picks the period with the freshest
/// home-area print so the line is never empty just because the
/// front contract hasn't gated yet.
fn effective_period_ms(
    trades: &[PublicTrade],
    home: &[&str],
    window_ms: f64,
    pinned: Option<&str>,
) -> Option<f64> {
    if let Some(p) = pinned {
        let ms = Date::parse(p);
        if ms.is_finite() {
            return Some(ms);
        }
    }
    let now = Date::now();
    let tmin = now - window_ms;
    let mut latest_period: Option<f64> = None;
    let mut latest_exec = f64::NEG_INFINITY;
    for t in trades {
        if !home.contains(&t.buy_area.as_str()) && !home.contains(&t.sell_area.as_str()) {
            continue;
        }
        let exec = Date::parse(&t.execution_time);
        if !exec.is_finite() || exec < tmin {
            continue;
        }
        if exec > latest_exec {
            latest_exec = exec;
            latest_period = Some(Date::parse(&t.period));
        }
    }
    latest_period.or_else(|| {
        let fallback = (now / PERIOD_STEP_MS).ceil() * PERIOD_STEP_MS;
        Some(fallback)
    })
}

fn effective_period_iso(
    trades: &[PublicTrade],
    home: &[&str],
    window_ms: f64,
    pinned: Option<&str>,
) -> Option<String> {
    effective_period_ms(trades, home, window_ms, pinned)
        .map(|ms| Date::new(&ms.into()).to_iso_string().into())
}

/// One smoothed point on the rendered line.
struct Point {
    t: f64,
    price: f64,
}

fn buckets(prices: &[PublicTrade], home: &[&str], window_ms: f64, period_ms: f64) -> Vec<Point> {
    let now = Date::now();
    let tmin = now - window_ms;
    // Pick a bucket size so the window holds ~60 buckets — enough
    // resolution to see the shape, sparse enough that noise averages
    // out per bucket. Snapped to whole seconds.
    let bucket_ms = (window_ms / 60.0 / 1000.0).round().max(5.0) * 1000.0;
    let mut sums: BTreeMap<i64, (f64, u32)> = BTreeMap::new();
    for t in prices {
        if !home.contains(&t.buy_area.as_str()) && !home.contains(&t.sell_area.as_str()) {
            continue;
        }
        let ms = Date::parse(&t.execution_time);
        let pms = Date::parse(&t.period);
        if !ms.is_finite() || !pms.is_finite() || ms < tmin || pms != period_ms {
            continue;
        }
        let Ok(price) = t.price.parse::<f64>() else { continue };
        let b = (ms / bucket_ms).floor() as i64;
        let e = sums.entry(b).or_insert((0.0, 0));
        e.0 += price;
        e.1 += 1;
    }
    let mut out = Vec::with_capacity(sums.len());
    let mut smoothed: Option<f64> = None;
    for (b, (sum, count)) in sums {
        let mean = sum / count as f64;
        let s = match smoothed {
            Some(prev) => EMA_ALPHA * mean + (1.0 - EMA_ALPHA) * prev,
            None => mean,
        };
        smoothed = Some(s);
        out.push(Point {
            t: b as f64 * bucket_ms + bucket_ms / 2.0,
            price: s,
        });
    }
    out
}

fn draw(
    canvas: &web_sys::HtmlCanvasElement,
    trades: &[PublicTrade],
    window_ms: f64,
    pinned: Option<&str>,
) {
    let Some(parent) = canvas.parent_element() else { return };
    let dpr = web_sys::window().map(|w| w.device_pixel_ratio()).unwrap_or(1.0);
    let css_w = (parent.client_width() - 20).max(1) as f64;
    let css_h = 280.0;
    canvas.set_attribute("style", &format!("width:{css_w}px;height:{css_h}px;")).ok();
    canvas.set_width((css_w * dpr).floor().max(1.0) as u32);
    canvas.set_height((css_h * dpr).floor().max(1.0) as u32);

    let Ok(Some(ctx_obj)) = canvas.get_context("2d") else { return };
    let Ok(ctx) = ctx_obj.dyn_into::<CanvasRenderingContext2d>() else { return };
    ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0).ok();
    ctx.clear_rect(0.0, 0.0, css_w, css_h);

    // Padding leaves room for the y-axis labels on the left and the
    // x-axis "back N min" labels along the bottom.
    let pad_l = 38.0;
    let pad_r = 8.0;
    let pad_t = 6.0;
    let pad_b = 18.0;
    let inner_w = css_w - pad_l - pad_r;
    let inner_h = css_h - pad_t - pad_b;

    let home = home_areas();
    let period_ms = match effective_period_ms(trades, &home, window_ms, pinned) {
        Some(p) => p,
        None => return,
    };
    let line = buckets(trades, &home, window_ms, period_ms);

    let now = Date::now();
    let tmin = now - window_ms;
    let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
    for p in &line {
        ymin = ymin.min(p.price);
        ymax = ymax.max(p.price);
    }
    if !ymin.is_finite() {
        // Flat placeholder range so the axes still draw before any
        // data lands.
        ymin = 70.0;
        ymax = 100.0;
    }
    if ymax - ymin < 1.0 {
        ymax = ymin + 1.0;
    }
    let ypad = (ymax - ymin) * 0.1;
    ymin -= ypad;
    ymax += ypad;
    let xs = |t: f64| pad_l + ((t - tmin) / window_ms) * inner_w;
    let ys = |p: f64| pad_t + (1.0 - (p - ymin) / (ymax - ymin)) * inner_h;

    // Axes: horizontal grid lines + price labels left, time labels
    // along the bottom.
    ctx.set_stroke_style_str(BORDER_COLOR);
    ctx.set_fill_style_str(MUTED_COLOR);
    ctx.set_font("10px ui-monospace, monospace");
    ctx.set_line_width(1.0);
    let y_range = ymax - ymin;
    let y_decimals: usize = if y_range >= 10.0 { 0 } else if y_range >= 2.0 { 1 } else { 2 };
    for i in 0..=Y_STEPS {
        let y = pad_t + (i as f64 / Y_STEPS as f64) * inner_h;
        ctx.begin_path();
        ctx.move_to(pad_l, y);
        ctx.line_to(css_w - pad_r, y);
        ctx.stroke();
        let v = ymax - (i as f64 / Y_STEPS as f64) * (ymax - ymin);
        ctx.set_text_align("right");
        ctx.fill_text(&format!("{v:.*}", y_decimals), pad_l - 4.0, y + 3.0).ok();
    }
    ctx.set_text_align("center");
    let x_step = pick_x_step(window_ms);
    let mut offset = 0.0;
    while offset <= window_ms {
        let t = now - offset;
        let x = xs(t);
        let mins = (offset / 60_000.0).round() as i64;
        ctx.fill_text(&format_back(mins), x, css_h - 4.0).ok();
        offset += x_step;
    }

    if line.len() < 2 {
        return;
    }
    ctx.set_stroke_style_str(LINE_COLOR);
    ctx.set_line_width(2.0);
    ctx.set_line_join("round");
    ctx.begin_path();
    for (i, p) in line.iter().enumerate() {
        let x = xs(p.t);
        let y = ys(p.price);
        if i == 0 {
            ctx.move_to(x, y);
        } else {
            ctx.line_to(x, y);
        }
    }
    ctx.stroke();
}
