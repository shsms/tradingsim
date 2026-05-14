//! Scenario registry — a small piece of mutable state the lisp side
//! populates via `(define-scenario …)` and the UI layer reads/writes
//! via the HTTP endpoints. Stages name a tulisp defun the HTTP layer
//! invokes through the shared TulispContext when the user clicks
//! Start / Next from the browser.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;

#[derive(Clone, Debug)]
pub struct Stage {
    /// Display name shown in the UI ("06:00 morning ramp").
    pub name: String,
    /// Wallclock UTC hour window this stage represents.
    pub hour_from: f64,
    pub hour_to: f64,
    /// Aggressor side-bias at the start and end of the window.
    /// The tick interpolates linearly between them as the wallclock
    /// advances through [hour_from, hour_to).
    pub bias_from: f64,
    pub bias_to: f64,
}

#[derive(Clone, Debug)]
pub struct ScenarioDef {
    pub name: String,
    pub description: String,
    pub stages: Vec<Stage>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScenarioRuntime {
    /// `None` = not running; `Some(i)` = stage `i` is current.
    pub current_stage: Option<usize>,
    pub started_at: Option<DateTime<Utc>>,
    pub stage_entered_at: Option<DateTime<Utc>>,
    /// True when the operator jumped away from the wallclock-current
    /// stage. The tick task respects this and stops auto-advancing
    /// until the operator returns to the wallclock-matching stage
    /// or restarts the scenario.
    pub manual_override: bool,
}

#[derive(Clone, Debug)]
pub struct ScenarioEntry {
    pub def: ScenarioDef,
    pub runtime: ScenarioRuntime,
}

pub type SharedScenarios = Arc<Mutex<HashMap<String, ScenarioEntry>>>;

pub fn new_registry() -> SharedScenarios {
    Arc::new(Mutex::new(HashMap::new()))
}
