//! Price tape chart — canvas-based line over the last N minutes
//! of public-trade prints, bucketed + EMA-smoothed. First slice
//! plots the line only; axes / labels / window + delivery selectors
//! land in follow-ups.

use std::collections::BTreeMap;

use gloo_timers::future::TimeoutFuture;
use js_sys::Date;
use leptos::html::Canvas;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::CanvasRenderingContext2d;

use crate::types::PublicTrade;
use crate::util::{ALL_AREAS, AreaGroup};

const WINDOW_MS: f64 = 30.0 * 60.0 * 1000.0;
const PERIOD_STEP_MS: f64 = 15.0 * 60.0 * 1000.0;
const EMA_ALPHA: f64 = 0.4;
const LINE_COLOR: &str = "#58a6ff";
const BORDER_COLOR: &str = "#30363d";
const MUTED_COLOR: &str = "#8b949e";
const Y_STEPS: i32 = 5;

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

#[component]
pub fn PriceChart() -> impl IntoView {
    let trades = expect_context::<ReadSignal<Vec<PublicTrade>>>();
    let canvas_ref = NodeRef::<Canvas>::new();

    // 1 s redraw tick; throttle vs the WS print rate (the resampler
    // averages within the bucket anyway, so per-print redraws would
    // just thrash the GPU).
    leptos::task::spawn_local(async move {
        loop {
            if let Some(canvas) = canvas_ref.get_untracked() {
                trades.with_untracked(|v| draw(&canvas, v));
            }
            TimeoutFuture::new(1000).await;
        }
    });

    view! {
        <section class="panel panel-chart">
            <div class="chart-head">
                <h2>"Price tape"</h2>
            </div>
            <canvas node_ref=canvas_ref id="price-chart"></canvas>
        </section>
    }
}

fn home_areas() -> Vec<&'static str> {
    ALL_AREAS
        .iter()
        .filter(|a| a.group == AreaGroup::Home)
        .map(|a| a.code)
        .collect()
}

/// Effective delivery period (epoch ms) the chart pins on. Picks the
/// period with the most recent home-area print so the line is never
/// empty just because the front contract hasn't gated yet; falls
/// back to the soonest upcoming 15-min boundary when there's no
/// activity in the window.
fn effective_period_ms(trades: &[PublicTrade], home: &[&str], window_ms: f64) -> f64 {
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
    latest_period.unwrap_or_else(|| (now / PERIOD_STEP_MS).ceil() * PERIOD_STEP_MS)
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
        let ms = parse_ms(&t.execution_time);
        let pms = parse_ms(&t.period);
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

fn parse_ms(iso: &str) -> f64 {
    Date::parse(iso)
}

fn draw(canvas: &web_sys::HtmlCanvasElement, trades: &[PublicTrade]) {
    let Some(parent) = canvas.parent_element() else { return };
    let dpr = web_sys::window().and_then(|w| Some(w.device_pixel_ratio())).unwrap_or(1.0);
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
    let period_ms = effective_period_ms(trades, &home, WINDOW_MS);
    let line = buckets(trades, &home, WINDOW_MS, period_ms);

    let now = Date::now();
    let tmin = now - WINDOW_MS;
    let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
    for p in &line {
        ymin = ymin.min(p.price);
        ymax = ymax.max(p.price);
    }
    if !ymin.is_finite() {
        // Flat placeholder range so the axes still draw before any
        // data lands. JS UI does the same so the panel doesn't pop.
        ymin = 70.0;
        ymax = 100.0;
    }
    if ymax - ymin < 1.0 {
        ymax = ymin + 1.0;
    }
    let ypad = (ymax - ymin) * 0.1;
    ymin -= ypad;
    ymax += ypad;
    let xs = |t: f64| pad_l + ((t - tmin) / WINDOW_MS) * inner_w;
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
    let x_step = pick_x_step(WINDOW_MS);
    let mut offset = 0.0;
    while offset <= WINDOW_MS {
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
