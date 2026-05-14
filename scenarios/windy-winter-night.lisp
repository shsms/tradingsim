;; scenarios/windy-winter-night.lisp — wind production runs hot
;; overnight and into the morning. Surplus tilts bias sell-heavy at
;; night; daytime curves stay flat and modest.

(define-scenario
 :name "windy-winter-night"
 :description "Wind surplus overnight; modest daytime swings. Bias 0.42 → 0.62."
 :date "2026-02-10"  ;; late winter Atlantic storm — wind, no sun
 :stages
 '((:name "overnight"     :hour-from 0.0  :hour-to 5.0  :bias-from 0.42 :bias-to 0.42
    :cloud-cover 0.55 :mean-wind 13.0 :temperature-base 273.0)
   (:name "dawn ramp"     :hour-from 5.0  :hour-to 8.0  :bias-from 0.42 :bias-to 0.55
    :cloud-cover 0.55 :mean-wind 12.0 :temperature-base 273.0)
   (:name "morning peak"  :hour-from 8.0  :hour-to 10.0 :bias-from 0.55 :bias-to 0.50
    :cloud-cover 0.50 :mean-wind 10.0 :temperature-base 274.0)
   (:name "plateau"       :hour-from 10.0 :hour-to 13.0 :bias-from 0.50 :bias-to 0.45
    :cloud-cover 0.50 :mean-wind 8.0 :temperature-base 276.0)
   (:name "plateau"       :hour-from 13.0 :hour-to 16.0 :bias-from 0.45 :bias-to 0.45
    :cloud-cover 0.50 :mean-wind 7.0 :temperature-base 277.0)
   (:name "evening ramp"  :hour-from 16.0 :hour-to 18.0 :bias-from 0.45 :bias-to 0.55
    :cloud-cover 0.55 :mean-wind 8.0 :temperature-base 275.0)
   (:name "evening peak"  :hour-from 18.0 :hour-to 21.0 :bias-from 0.55 :bias-to 0.62
    :cloud-cover 0.55 :mean-wind 10.0 :temperature-base 273.0)
   (:name "wind-down"     :hour-from 21.0 :hour-to 23.0 :bias-from 0.62 :bias-to 0.48
    :cloud-cover 0.55 :mean-wind 12.0 :temperature-base 272.0)
   (:name "late night"    :hour-from 23.0 :hour-to 24.0 :bias-from 0.48 :bias-to 0.42
    :cloud-cover 0.55 :mean-wind 13.0 :temperature-base 271.0)))
