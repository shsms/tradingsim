;; scenarios/weather-shift.lisp — overcast morning that clears
;; mid-day. Cloud cover starts at 0.85 (forecasts predict thin
;; solar, midday belly stays shallow); a 5-minute one-shot timer
;; flips it to 0.10 (sudden clearing). Trading apps subscribed to
;; the WeatherForecastService see the revision land within their
;; next 60-second forecast emit, and the bias tick re-derives the
;; per-quarter MM reference 5 seconds after the flip — so the
;; full path from weather change → forecast revision → price
;; impact is exercised end-to-end.
;;
;; This is a *load-time* script, not a TimeOfDay scenario; it
;; doesn't appear in the UI's scenarios panel. To activate, add
;; (load "scenarios/weather-shift.lisp") to config.lisp.

(load "sim/common.lisp")

(set-weather-cloud-cover 0.85)
(log.info "weather-shift: armed at cloud_cover=0.85; clearing in 5 min")

(setq active-timers
      (cons (run-with-timer
             300.0 0
             (lambda ()
               (set-weather-cloud-cover 0.10)
               (log.info "weather-shift: clouds lifted; cloud_cover=0.10")))
            active-timers))
