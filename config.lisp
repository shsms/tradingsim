;; Sample tradingsim config — loaded by the binary at startup.
;;
;; A notify-rs watcher reloads this file (plus anything passed to
;; (watch-file …)) on save: re-firing (%make-mm-fleet …) /
;; (%make-aggressor-fleet …) for each named fleet rewrites the
;; SharedFleetParams in place, so running per-contract MMs and
;; aggressors pick up new bands / rates on their next tick without
;; restart. Spawn-time fields (window_quarters, seed_base) and the
;; binary's socket addresses stay frozen — those need a relaunch.

(unless (boundp 'tradingsim-loaded)
  (setq tradingsim-loaded t)
  (load "sim/common.lisp"))

;; Cancel any timers from a previous load before the new config
;; re-registers them. Required for hot reload to start clean.
(reset-state)

;; Watch the support files so saving them also triggers a reload.
(watch-file "sim/common.lisp")

(set-trading-addr "[::1]:8810")
(set-ui-addr "127.0.0.1:8811")
(set-weather-addr "[::1]:8820")
(set-physics-tick-ms 100)

;; Bias-scale knob — EUR per (bias - 0.5) unit pushed into the MM's
;; demand + surplus tilt. Higher = more dramatic price moves under a
;; lopsided scenario stage. Rust falls back to 25.0 if this line is
;; absent; tune up to 30-40 if you want negative prices faster.
(set-mm-bias-scale 25.0)

;; MM refresh cadence (ms) — how often every market-maker re-prices
;; and re-posts its bid+ask. Floor 100 ms, default 2000 ms. Lower
;; = quote chop is more visible + price catches up to scenario
;; shifts faster; higher = calmer book, longer windows where
;; aggressors lift one stale ask before it moves. Hot-reloadable:
;; saving this file re-runs each (mm-fleet …) below, which writes
;; the new value into the fleet's shared params; per-contract MMs
;; pick it up on their next refresh.
(setq mm-refresh-ms 2000)

;; --- TSO regions ----------------------------------------------------------
;;
;; Four German TSO control zones treated as separate delivery areas
;; for redispatch / physical purposes — each carries its own weather
;; location and its own aggressor fleet so per-region liquidity
;; profiles stay observable. The MM, on the other hand, is one fleet
;; covering all four areas at the same price: Germany is a single
;; bidding zone for both day-ahead and intraday wholesale, so quotes
;; should clear at one national number regardless of which TSO owns
;; the wire. The fleet's reference baseline is the average of
;; effective_ref across the four weather locations.
;;
;; Per row:
;;   (eic, prefix, ag-sizes-per-profile)

(setq areas
      '(("10YDE-EON------1"   "tn"  (0.3 0.7 1.4 2.0))
        ("10YDE-RWENET---I"   "am"  (0.2 0.5 1.0 1.4))
        ("10YDE-VE-------2"   "hz"  (0.2 0.3 0.6 0.9))
        ("10YDE-ENBW-----N"   "bw"  (0.1 0.2 0.3 0.4))))

;; Markets + a single multi-area gridpool + all-pairs SIDC coupling.
(register-markets (mapcar 'car areas))
(%make-gridpool :id 1 :name "default" :areas (mapcar 'car areas))
(couple-all-pairs (mapcar 'car areas))

;; One national MM fleet covering all four DE control zones at the
;; same price. The size table is the aggregate of what the four
;; per-area fleets used to carry (sum of front bands ≈ 3.6 MW, etc.),
;; so total liquidity per book matches the old per-area depth while
;; pricing is unified.
(mm-fleet :areas (mapcar 'car areas)
          :prefix "de"
          :sizes '(3.6 2.7 1.8 1.1)
          :refresh-ms mm-refresh-ms)

;; Per-area aggressor fleets keep the per-region volume profile
;; (TN ~40%, AM ~30%, HZ ~20%, BW ~10%). rates-base doubled vs the
;; aggressor-fleet defaults (500 1500 3500 8000) — calmer trade
;; tape (~half the prints per second). Hot-reloadable: saving this
;; file re-runs the fleet primitive, which writes the new rates
;; into the fleet's shared params; per-contract aggressors apply
;; them on their next fire.
(dolist (entry areas)
  (aggressor-fleet :area (car entry)
                   :prefix (cadr entry)
                   :sizes (caddr entry)
                   :rates-base '(1000 3000 7000 16000)))

;; --- International coupling ----------------------------------------------
;;
;; Four neighbouring bidding zones, each coupled to every German TSO
;; area via SIDC. Real SIDC closes the cross-border route 60 min
;; before delivery; :gate-offset-seconds 3600 below tells the
;; matcher to stop crossing the edge by then. Inside Germany,
;; trading continues until the regular gate.

(setq international-areas
      '(("10YFR-RTE------C"   "fr")    ;; France
        ("10YNL----------L"   "nl")    ;; Netherlands
        ("10YBE----------2"   "be")    ;; Belgium
        ("10YAT-APG------L"   "at")))  ;; Austria

(register-markets (mapcar 'car international-areas))

;; Smaller fleets — 4 MMs covering the next hour, two aggressor
;; profiles per contract. Enough to demonstrate cross-border
;; matching without doubling the task count. Per-area references
;; come from the shared forward curve evaluated at each fleet's
;; weather location (lower cloud cover / higher wind drags the
;; reference down naturally), so no per-fleet :reference-base
;; override is needed.
(dolist (entry international-areas)
  (mm-fleet :area (car entry)
            :prefix (cadr entry)
            :quarters 4
            :sizes '(0.5 0.4 0.3 0.2)
            :refresh-ms mm-refresh-ms)
  (aggressor-fleet :area (car entry)
                   :prefix (cadr entry)
                   :quarters 4
                   :sizes '(0.1 0.2)
                   :rates-base '(3000 8000)))

;; Re-register the gridpool with all eight areas so the user can
;; place orders in any of them through the same default pool.
(%make-gridpool
 :id 1
 :name "default"
 :areas (append (mapcar 'car areas) (mapcar 'car international-areas)))

;; Every DE TSO zone ↔ every international area, 60-min gate.
(couple-pairs-across (mapcar 'car areas)
                     (mapcar 'car international-areas)
                     :gate-offset-seconds 3600)

;; --- Per-area weather locations ------------------------------------------
;;
;; Each delivery area gets its own atmospheric anchor (0.1°
;; lat/lon granularity). Bias tick reads weather via
;; weather.for_area(area_code); WeatherForecastService streams one
;; LocationForecast per registered slot. Trading apps that ask the
;; weather API for a specific lat/lon get the nearest registered
;; entry.

(%make-weather-location :name "tn" :area "10YDE-EON------1"
                        :lat 50.4 :lon 11.6
                        :cloud-cover 0.35 :mean-wind 5.0)
(%make-weather-location :name "am" :area "10YDE-RWENET---I"
                        :lat 51.2 :lon  7.0
                        :cloud-cover 0.40 :mean-wind 5.0)
(%make-weather-location :name "hz" :area "10YDE-VE-------2"
                        :lat 52.5 :lon 13.4
                        :cloud-cover 0.25 :mean-wind 6.5)
(%make-weather-location :name "bw" :area "10YDE-ENBW-----N"
                        :lat 48.8 :lon  9.2
                        :cloud-cover 0.30 :mean-wind 4.5)
(%make-weather-location :name "fr" :area "10YFR-RTE------C"
                        :lat 48.9 :lon  2.3
                        :cloud-cover 0.35 :mean-wind 5.5)
(%make-weather-location :name "nl" :area "10YNL----------L"
                        :lat 52.4 :lon  4.9
                        :cloud-cover 0.55 :mean-wind 7.0)
(%make-weather-location :name "be" :area "10YBE----------2"
                        :lat 50.8 :lon  4.4
                        :cloud-cover 0.45 :mean-wind 6.0)
(%make-weather-location :name "at" :area "10YAT-APG------L"
                        :lat 48.2 :lon 16.4
                        :cloud-cover 0.30 :mean-wind 4.0)

;; --- Demand / surplus tilts ------------------------------------------------
;;
;; Per-MM tilts are no longer addressable by name — contract-owned MMs
;; rotate through the window, so a stable handle would map to a moving
;; contract. Use the scenario layer instead: a (define-scenario …) stage
;; with bias-from/bias-to ≠ 0.5 produces the same demand+surplus shift
;; across the affected quarters, and the shift fades naturally with
;; offset-to-gate via the decay-weight blend.

;; --- Scenarios -------------------------------------------------------------
;;
;; Time-of-day scenarios live in scenarios/ and load on demand. The
;; default behaviour (no scenario active) already applies a natural
;; duck curve to every aggressor; scenarios just override the
;; near-term shape so the orderbook looks like a different point in
;; the day.
;;
(load "scenarios/sunny-summer-day.lisp")
(load "scenarios/rainy-summer-day.lisp")
(load "scenarios/sunny-summer-holiday.lisp")
(load "scenarios/winter-weekday.lisp")
(load "scenarios/windy-winter-night.lisp")
(load "scenarios/scarcity-spike.lisp")
(load "scenarios/buy-only-flow.lisp")

;; Load-time edge cases (not in the UI scenarios panel). Uncomment
;; to activate at boot:
;; (load "scenarios/weather-shift.lisp")
;; (load "scenarios/day-ahead-print.lisp")
