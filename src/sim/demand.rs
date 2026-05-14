//! Market-demand shape — pure functions that the scenario bias
//! tick reads each cycle. Lifted out of `scenarios.rs` so the
//! scenarios module can stay focused on the registry / runtime /
//! stage transitions, and the duck-curve mechanics can grow
//! (or get re-tuned) on their own.

/// Aggressor side-bias for a given hour-of-day under the natural
/// (no-scenario) duck curve. Returns values in [0, 1] — 0.5 is
/// balanced flow, < 0.5 sell-heavy, > 0.5 buy-heavy. Reflects a
/// median weekday: overnight balanced, morning ramp climbs into a
/// short post-peak settle, midday belly tilts sell-heavy as solar
/// floods supply, late-afternoon transition, evening peak goes
/// strongly buy-side, late-evening cooling.
pub fn natural_duck_bias(hour: f64) -> f64 {
    let h = hour.rem_euclid(24.0);
    match h {
        h if h < 5.0 => 0.50,                              // overnight
        h if h < 9.0 => lerp(0.50, 0.62, (h - 5.0) / 4.0), // morning ramp
        h if h < 10.0 => 0.55,                             // post-peak
        h if h < 15.0 => 0.35,                             // solar belly
        h if h < 17.0 => 0.50,                             // transition
        h if h < 21.0 => 0.72,                             // evening peak
        h if h < 23.0 => 0.60,
        _ => 0.50,
    }
}

/// How strongly a scenario's bias override applies at quarter-offset
/// `i`. q0 = 1.0, decays as `exp(-i/12)` so q12 (3 h out) ~= 0.37
/// and q47 (~12 h out) ~= 0.02. Keeps a scenario's near-term
/// effect crisp without rewriting the entire forward curve.
pub fn decay_weight(offset: i64) -> f64 {
    (-(offset.max(0) as f64) / 12.0).exp()
}

/// Linear interpolation. `t` outside [0, 1] extrapolates; callers
/// clamp where they care.
pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}
