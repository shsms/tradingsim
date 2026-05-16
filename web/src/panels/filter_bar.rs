//! Area-filter chips. Drives which weather rows + public-trade
//! prints stay visible. State is shared via context as a Leptos
//! `RwSignal<FilterState>`; toggling a chip persists to
//! `localStorage` so reloads keep the user's last selection.

use std::collections::HashSet;

use leptos::prelude::*;

use crate::intl::short_time;
use crate::util::{ALL_AREAS, AreaGroup};

#[derive(Debug, Clone)]
pub struct FilterState {
    /// EIC codes currently visible. Defaults to the home zones; the
    /// neighbours toggle flips the four cross-border zones on/off
    /// in lockstep.
    pub active_areas: HashSet<&'static str>,
    pub show_neighbours: bool,
}

impl Default for FilterState {
    fn default() -> Self {
        Self {
            active_areas: ALL_AREAS
                .iter()
                .filter(|a| a.group == AreaGroup::Home)
                .map(|a| a.code)
                .collect(),
            show_neighbours: false,
        }
    }
}

const KEY_ACTIVE: &str = "tradingsim-active-areas";
const KEY_NEIGHBOURS: &str = "tradingsim-neighbours";

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

pub fn load_filter() -> FilterState {
    let mut state = FilterState::default();
    let Some(ls) = local_storage() else {
        return state;
    };
    if let Ok(Some(raw)) = ls.get_item(KEY_ACTIVE) {
        state.active_areas = raw
            .split(',')
            .filter(|c| !c.is_empty())
            // Reject any persisted code that no longer matches a
            // configured area — keeps stale entries from drifting in.
            .filter_map(|c| ALL_AREAS.iter().find(|a| a.code == c).map(|a| a.code))
            .collect();
    }
    if let Ok(Some(raw)) = ls.get_item(KEY_NEIGHBOURS) {
        state.show_neighbours = raw == "true";
    }
    state
}

fn save_filter(state: &FilterState) {
    let Some(ls) = local_storage() else { return };
    let codes: Vec<&str> = state.active_areas.iter().copied().collect();
    let _ = ls.set_item(KEY_ACTIVE, &codes.join(","));
    let _ = ls.set_item(
        KEY_NEIGHBOURS,
        if state.show_neighbours { "true" } else { "false" },
    );
}

/// Visible while a trade-row click has pinned a delivery period. Click
/// the pill to clear the pin. Sits above the area chips because the
/// focused-period scope crosses every panel (chart + trades),
/// whereas the area chips only scope the panels below them.
#[component]
pub fn ContractPill() -> impl IntoView {
    let focused = expect_context::<RwSignal<Option<String>>>();
    let display_tz = expect_context::<RwSignal<String>>();
    let clear = move |_| focused.set(None);
    let body = move || {
        let p = focused.get()?;
        let tz = display_tz.get();
        let label = short_time(&p, &tz);
        Some(view! {
            <section class="filter-bar" aria-label="active delivery">
                <span>"focused"</span>
                <span class="contract-pill" on:click=clear>
                    <span>{label}</span>
                    <span>"×"</span>
                </span>
            </section>
        })
    };
    view! { {body} }
}

#[component]
pub fn FilterBar() -> impl IntoView {
    let state = expect_context::<RwSignal<FilterState>>();

    let chips = move || {
        let s = state.get();
        let mut visible: Vec<&'static crate::util::AreaSpec> = ALL_AREAS
            .iter()
            .filter(|a| a.group == AreaGroup::Home || s.show_neighbours)
            .collect();
        // ALL_AREAS already sorts home-first; keep that order.
        visible.sort_by_key(|a| match a.group {
            AreaGroup::Home => 0,
            AreaGroup::Neighbour => 1,
        });
        visible
            .into_iter()
            .map(|a| {
                let code = a.code;
                let active = s.active_areas.contains(code);
                let cls = if active { "chip active" } else { "chip" };
                let toggle = move |_| {
                    state.update(|s| {
                        if s.active_areas.contains(code) {
                            s.active_areas.remove(code);
                        } else {
                            s.active_areas.insert(code);
                        }
                        save_filter(s);
                    });
                };
                view! { <span class=cls on:click=toggle>{a.tag}</span> }
            })
            .collect_view()
    };

    let neighbours_chip = move || {
        let s = state.get();
        let label = if s.show_neighbours { "− neighbours" } else { "+ neighbours" };
        let cls = if s.show_neighbours { "chip active" } else { "chip" };
        let toggle = move |_| {
            state.update(|s| {
                s.show_neighbours = !s.show_neighbours;
                // Sync chip set to the toggle: neighbours on = all
                // four added, neighbours off = all four removed.
                for a in ALL_AREAS
                    .iter()
                    .filter(|a| a.group == AreaGroup::Neighbour)
                {
                    if s.show_neighbours {
                        s.active_areas.insert(a.code);
                    } else {
                        s.active_areas.remove(a.code);
                    }
                }
                save_filter(s);
            });
        };
        view! { <span class=cls on:click=toggle>{label}</span> }
    };

    view! {
        <section class="filter-bar" aria-label="area filter">
            <span>"areas"</span>
            <span id="filter-chips">
                {chips}
                {neighbours_chip}
            </span>
        </section>
    }
}
