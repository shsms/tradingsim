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
    /// Display name shown in the UI.
    pub name: String,
    /// Name of the tulisp defun the UI invokes to apply this stage.
    pub fn_name: String,
}

#[derive(Clone, Debug)]
pub struct ScenarioDef {
    pub name: String,
    pub description: String,
    pub stages: Vec<Stage>,
    /// Optional cleanup defun the UI invokes on POST /stop. Lets a
    /// scenario undo whatever Start did (cancel a timer, reset knobs)
    /// without forcing the operator to advance through every stage.
    pub on_stop_fn: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScenarioRuntime {
    /// `None` = not running; `Some(i)` = stage `i` is current.
    pub current_stage: Option<usize>,
    pub started_at: Option<DateTime<Utc>>,
    pub stage_entered_at: Option<DateTime<Utc>>,
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
