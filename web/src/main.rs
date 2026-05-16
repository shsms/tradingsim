#![allow(non_snake_case)] // Leptos components are PascalCase by convention.

use leptos::prelude::*;

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(|| view! { <Shell/> });
}

#[component]
fn Shell() -> impl IntoView {
    view! {
        <header class="page-header">
            <h1>"tradingsim"</h1>
            <span class="page-meta muted">"leptos shell — port in progress"</span>
        </header>
    }
}
