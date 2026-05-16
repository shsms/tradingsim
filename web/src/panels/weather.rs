//! /api/weather polled grid + click-to-expand detail per cell.

use std::time::Duration;

use gloo_net::http::Request;
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;

use crate::panels::filter_bar::FilterState;
use crate::types::WeatherLoc;
use crate::util::area_tag;

const WEATHER_POLL: Duration = Duration::from_secs(10);

async fn fetch_weather() -> Option<Vec<WeatherLoc>> {
    Request::get("/api/weather").send().await.ok()?.json().await.ok()
}

#[component]
pub fn Weather() -> impl IntoView {
    let (locs, set_locs) = signal(Vec::<WeatherLoc>::new());
    let (loaded, set_loaded) = signal(false);
    let filter = expect_context::<RwSignal<FilterState>>();
    let weather_loaded = expect_context::<RwSignal<bool>>();

    leptos::task::spawn_local(async move {
        loop {
            let got = fetch_weather().await;
            let success = got.is_some();
            if let Some(list) = got {
                set_locs.set(list);
                set_loaded.set(true);
                weather_loaded.set(true);
            }
            // Fast retry until the first successful fetch lands so a
            // transient hiccup at startup doesn't leave the panel
            // staring at "loading…" for a full poll interval.
            let delay = if success {
                WEATHER_POLL.as_millis() as u32
            } else {
                1000
            };
            TimeoutFuture::new(delay).await;
        }
    });

    let body = move || {
        let active = filter.with(|f| f.active_areas.clone());
        let list: Vec<_> = locs
            .get()
            .into_iter()
            // Hide the unlinked fallback location (no area_code) and
            // any location whose area isn't on the active-chips list.
            .filter(|l| {
                l.area_code
                    .as_deref()
                    .map(|c| active.contains(c))
                    .unwrap_or(false)
            })
            .collect();
        if list.is_empty() {
            return view! {
                <i class="muted">
                    {move || if loaded.get() {
                        "no weather for the active areas"
                    } else {
                        "loading…"
                    }}
                </i>
            }
            .into_any();
        }
        view! {
            <div class="weather-grid">
                {list.into_iter().map(|l| view! { <WeatherCell loc=l/> }).collect_view()}
            </div>
        }
        .into_any()
    };

    view! {
        <section class="panel panel-weather">
            <h2>"Weather (now)"</h2>
            <div class="muted" style="margin-bottom:6px">
                "Rows match the active area chips. Click a row for lat/lon "
                "and wind direction."
            </div>
            {body}
        </section>
    }
}

#[component]
fn WeatherCell(loc: WeatherLoc) -> impl IntoView {
    let (open, set_open) = signal(false);
    let tag = loc
        .area_code
        .as_deref()
        .map(area_tag)
        .unwrap_or("—")
        .to_string();
    view! {
        <div
            class="weather-cell"
            class:open=move || open.get()
            on:click=move |_| set_open.update(|o| *o = !*o)
        >
            <div class="weather-head">
                <span class="area-badge">{tag}</span>
                <span class="muted">{format!("☁ {:.2}", loc.cloud_cover)}</span>
            </div>
            <div class="weather-metric">
                "solar " <span class="muted">{format!("{} W/m²", loc.solar_now.round() as i64)}</span>
            </div>
            <div class="weather-metric">
                "wind " <span class="muted">{format!("{:.1} m/s", loc.wind_now)}</span>
            </div>
            <div class="weather-metric">
                "temp " <span class="muted">{format!("{:.1} °C", loc.temp_c_now)}</span>
            </div>
            <div class="weather-detail">
                {format!("lat {:.1} · lon {:.1}", loc.lat, loc.lon)}<br/>
                {format!("wind direction {}°", loc.wind_direction.round() as i64)}<br/>
                {format!("mean wind {:.1} m/s", loc.mean_wind)}
            </div>
        </div>
    }
}
