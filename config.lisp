;; Sample tradingsim config — loaded by the binary at startup.
;;
;; This file replaces the previous hardcoded defaults in
;; bin/tradingsim.rs. Edit and re-launch the binary to take effect;
;; hot reload (notify watcher) is on the deferred list.

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

;; --- TSO regions ----------------------------------------------------------
;;
;; Four German TSO control zones treated as separate delivery areas.
;; In reality such markets trade them as one DE-LU bidding zone; here we split
;; them so per-region liquidity profiles are observable. Volume share
;; is roughly TenneT ~40% > Amprion ~30% > 50Hertz ~20% > TransnetBW
;; ~10%, and the size tables below track that.
;;
;; Per row:
;;   (eic, prefix, mm-sizes-per-band, ag-sizes-per-profile)

(setq areas
      '(("10YDE-EON------1"   "tn"  (1.5 1.1 0.7 0.4)  (0.3 0.7 1.4 2.0))
        ("10YDE-RWENET---I"   "am"  (1.2 0.9 0.6 0.4)  (0.2 0.5 1.0 1.4))
        ("10YDE-VE-------2"   "hz"  (0.6 0.5 0.3 0.2)  (0.2 0.3 0.6 0.9))
        ("10YDE-ENBW-----N"   "bw"  (0.3 0.2 0.2 0.1)  (0.1 0.2 0.3 0.4))))

;; Markets + a single multi-area gridpool + all-pairs SIDC coupling.
(register-markets (mapcar 'car areas))
(%make-gridpool :id 1 :name "default" :areas (mapcar 'car areas))
(couple-all-pairs (mapcar 'car areas))

;; Per-area MM + aggressor fleets. Each area gets 48 MMs (one per
;; quarter, rolling forward) and 4 × 48 = 192 aggressors. Seeds are
;; auto-assigned per fleet call so RNG streams don't collide.
(dolist (entry areas)
  (mm-fleet :area (car entry)
            :prefix (cadr entry)
            :sizes (caddr entry))
  (aggressor-fleet :area (car entry)
                   :prefix (cadr entry)
                   :sizes (cadddr entry)))

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
;; profiles per quarter. Enough to demonstrate cross-border
;; matching without doubling the task count.
(dolist (entry international-areas)
  (mm-fleet :area (car entry)
            :prefix (cadr entry)
            :quarters 4
            :sizes '(0.5 0.4 0.3 0.2)
            :reference-base 75.0)
  (aggressor-fleet :area (car entry)
                   :prefix (cadr entry)
                   :quarters 4
                   :sizes '(0.1 0.2)
                   :rates-base '(1500 4000)))

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
;; Uncomment to skew an individual MM's quoting:
;;
;; (set-mm-demand "tn-q4" 0.20)    ;; TenneT q4: aggressive procurement
;; (set-mm-surplus "am-q3" 0.30)   ;; Amprion q3: midday solar dump

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
