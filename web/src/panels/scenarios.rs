//! Scenarios panel — reads the shared `RwSignal<Vec<Scenario>>` from
//! context (Shell owns the polling) and offers idle-picker / active
//! controls. Start / Prev / Next / Stop fire the matching POST then
//! immediately refresh the shared signal.

use gloo_net::http::Request;
use leptos::prelude::*;

use crate::intl::now_hour_in_tz;
use crate::types::{Scenario, Stage};

async fn fetch_scenarios() -> Option<Vec<Scenario>> {
    Request::get("/api/scenarios")
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()
}

async fn post_action(name: &str, action: &str) {
    let url = format!("/api/scenarios/{name}/{action}");
    let _ = Request::post(&url).send().await;
}

#[component]
pub fn Scenarios() -> impl IntoView {
    let scenarios = expect_context::<RwSignal<Vec<Scenario>>>();

    let act = move |name: String, action: &'static str| {
        leptos::task::spawn_local(async move {
            post_action(&name, action).await;
            if let Some(list) = fetch_scenarios().await {
                scenarios.set(list);
            }
        });
    };

    let switch_to = move |target: String| {
        leptos::task::spawn_local(async move {
            let list = fetch_scenarios().await.unwrap_or_default();
            if let Some(active) = list.iter().find(|s| s.current_stage.is_some()) {
                post_action(&active.name, "stop").await;
            }
            post_action(&target, "start").await;
            if let Some(list) = fetch_scenarios().await {
                scenarios.set(list);
            }
        });
    };

    let body = move || {
        let list = scenarios.get();
        if list.is_empty() {
            return view! { <i class="muted">"no scenarios registered"</i> }.into_any();
        }
        let active = list.iter().find(|s| s.current_stage.is_some()).cloned();
        match active {
            Some(a) => {
                let inactive: Vec<_> = list
                    .into_iter()
                    .filter(|s| s.current_stage.is_none())
                    .collect();
                view! { <ActiveScenarioRow scenario=a others=inactive on_action=act on_switch=switch_to/> }
                    .into_any()
            }
            None => view! { <IdlePicker scenarios=list on_start=act/> }.into_any(),
        }
    };

    view! {
        <section class="panel panel-scenarios">
            <h2>"Scenarios"</h2>
            {body}
        </section>
    }
}

#[component]
fn IdlePicker<F>(scenarios: Vec<Scenario>, on_start: F) -> impl IntoView
where
    F: Fn(String, &'static str) + Copy + 'static,
{
    let on_change = move |ev| {
        let name = event_target_value(&ev);
        if !name.is_empty() {
            on_start(name, "start");
        }
    };
    view! {
        <div class="scenario-picker">
            <span class="muted">"Start a scenario:"</span>
            <select on:change=on_change>
                <option value="">"— pick one —"</option>
                {scenarios.into_iter().map(|s| view! {
                    <option value=s.name.clone()>
                        {format!("{} — {}", s.name, s.description)}
                    </option>
                }).collect_view()}
            </select>
        </div>
    }
}

#[component]
fn ActiveScenarioRow<F, S>(
    scenario: Scenario,
    others: Vec<Scenario>,
    on_action: F,
    on_switch: S,
) -> impl IntoView
where
    F: Fn(String, &'static str) + Copy + 'static,
    S: Fn(String) + Copy + 'static,
{
    let sim_tz = expect_context::<RwSignal<String>>();
    let cur = scenario.current_stage.unwrap_or(0);
    let last = scenario.stages.len().saturating_sub(1);
    let n = scenario.name.clone();
    let header_name = scenario.name.clone();
    let prev_dis = cur == 0;
    let next_dis = cur >= last;

    let on_prev = {
        let n = n.clone();
        move |_| on_action(n.clone(), "prev")
    };
    let on_next = {
        let n = n.clone();
        move |_| on_action(n.clone(), "next")
    };
    let on_stop = {
        let n = n.clone();
        move |_| on_action(n.clone(), "stop")
    };
    let on_switch_change = move |ev| {
        let v = event_target_value(&ev);
        if !v.is_empty() {
            on_switch(v);
        }
    };

    // Stage-jump closure: name+idx in, async POST + refresh out.
    // Captures nothing reactively, so it's Copy and reusable across
    // every block / row click handler below.
    let scenarios_sig = expect_context::<RwSignal<Vec<Scenario>>>();
    let jump = move |name: String, idx: usize| {
        leptos::task::spawn_local(async move {
            let _ = Request::post(&format!("/api/scenarios/{name}/jump/{idx}"))
                .send()
                .await;
            if let Some(list) = fetch_scenarios().await {
                scenarios_sig.set(list);
            }
        });
    };

    let stages = scenario.stages.clone();
    let stage_blocks = stages
        .iter()
        .enumerate()
        .map(|(i, st)| {
            let name = scenario.name.clone();
            let left = (st.hour_from / 24.0) * 100.0;
            let width = ((st.hour_to - st.hour_from) / 24.0) * 100.0;
            let mut cls = String::from("timeline-stage");
            if i == cur {
                cls.push_str(" current");
            } else if i < cur {
                cls.push_str(" done");
            }
            let title = format!("{} — bias {:.2} → {:.2}", st.name, st.bias_from, st.bias_to);
            let on_block_click = move |_| jump(name.clone(), i);
            view! {
                <div
                    class=cls
                    style=format!("left:{left}%;width:{width}%")
                    title=title
                    on:click=on_block_click
                >
                    {(i + 1).to_string()}
                </div>
            }
        })
        .collect_view();
    let now_pct = move || (now_hour_in_tz(&sim_tz.get()) / 24.0) * 100.0;

    let stage_rows = stages
        .iter()
        .enumerate()
        .map(|(i, st)| {
            let name = scenario.name.clone();
            let mut cls = String::from("stage-list-row");
            let state = if i == cur {
                cls.push_str(" current");
                "▶"
            } else if i < cur {
                cls.push_str(" done");
                "✓"
            } else {
                ""
            };
            let time = format!(
                "{}–{}",
                fmt_stage_hour(st.hour_from),
                fmt_stage_hour(st.hour_to)
            );
            let bias = format!("{:.2} → {:.2}", st.bias_from, st.bias_to);
            let weather = stage_weather(st);
            let stage_name = st.name.clone();
            let title = stage_name.clone();
            let on_row_click = move |_| jump(name.clone(), i);
            view! {
                <div class=cls on:click=on_row_click title=title>
                    <span class="stage-num">{(i + 1).to_string()}</span>
                    <span class="stage-state">{state}</span>
                    <span class="stage-name">{stage_name}</span>
                    <span class="stage-time">{time}</span>
                    <span class="stage-bias">{bias}</span>
                    <span class="stage-weather">{weather}</span>
                </div>
            }
        })
        .collect_view();

    view! {
        <div class="scenario active">
            <div class="scenario-head">
                <strong>{header_name}</strong>
                {scenario.manual_override.then(|| view! {
                    <span class="badge-manual">"manual"</span>
                })}
                <span class="scenario-controls" style="margin-left:auto">
                    <button on:click=on_prev disabled=prev_dis>"Prev"</button>
                    <button on:click=on_next disabled=next_dis>"Next"</button>
                    <button on:click=on_stop>"Stop"</button>
                </span>
            </div>
            <div class="timeline">
                {stage_blocks}
                <div class="timeline-now" style=move || format!("left:{}%", now_pct())></div>
            </div>
            <div class="timeline-axis">
                <span>"00:00"</span><span>"06:00"</span><span>"12:00"</span>
                <span>"18:00"</span><span>"24:00"</span>
            </div>
            <div class="stage-list">
                <div class="stage-list-header">
                    <span></span>
                    <span></span>
                    <span class="stage-name">"stage"</span>
                    <span class="stage-time">"time"</span>
                    <span class="stage-bias">"bias"</span>
                    <span class="stage-weather">"weather"</span>
                </div>
                {stage_rows}
            </div>
            {(!others.is_empty()).then(|| view! {
                <div class="scenario-picker">
                    <span class="muted">"Switch to:"</span>
                    <select on:change=on_switch_change>
                        <option value="">"—"</option>
                        {others.into_iter().map(|s| {
                            view! { <option value=s.name.clone()>{s.name.clone()}</option> }
                        }).collect_view()}
                    </select>
                </div>
            })}
        </div>
    }
}

fn fmt_stage_hour(h: f64) -> String {
    let hh = h.floor() as i64;
    let mm = ((h - hh as f64) * 60.0).round() as i64;
    format!("{hh:02}:{mm:02}")
}

fn stage_weather(s: &Stage) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = s.cloud_cover {
        parts.push(format!("☁ {v:.2}"));
    }
    if let Some(v) = s.mean_wind {
        parts.push(format!("{v:.1} m/s"));
    }
    if let Some(v) = s.temperature_base {
        parts.push(format!("{} °C", (v - 273.15).round() as i64));
    }
    parts.join(" · ")
}
