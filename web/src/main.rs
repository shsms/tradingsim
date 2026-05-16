#![allow(non_snake_case)] // Leptos components are PascalCase by convention.

use gloo_net::http::Request;
use leptos::prelude::*;

mod types;
use types::{ClockResp, InfoResp};

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(|| view! { <Shell/> });
}

#[component]
fn Shell() -> impl IntoView {
    // Initial /api/info + /api/clock fetch. Both are fired once on
    // mount; refresh-on-tick comes when the header pulse bar lands.
    let info = LocalResource::new(|| async {
        Request::get("/api/info").send().await.ok()?.json::<InfoResp>().await.ok()
    });
    let clock = LocalResource::new(|| async {
        Request::get("/api/clock").send().await.ok()?.json::<ClockResp>().await.ok()
    });

    let info_line = move || match info.get().as_deref() {
        Some(Some(i)) => format!(
            "v{} · {} gridpool{} · {} markets · {} couplings",
            i.version,
            i.gridpools,
            if i.gridpools == 1 { "" } else { "s" },
            i.markets,
            i.couplings,
        ),
        Some(None) => "—".to_string(),
        None => "loading…".to_string(),
    };

    let tz_line = move || match clock.get().as_deref() {
        Some(Some(c)) => format!("tz: {}", c.tz),
        _ => String::new(),
    };

    view! {
        <header class="page-header">
            <h1>"tradingsim"</h1>
            <span class="page-meta muted">{info_line}</span>
            <span class="page-meta muted">{tz_line}</span>
        </header>
    }
}
