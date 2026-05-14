;; scenarios/rainy-summer-day.lisp — clouds dampen solar production,
;; so the midday belly stays shallow and the evening peak is modest.
;; Useful as the "what does the curve look like when the weather
;; misbehaves" foil for the sunny-summer scenarios.

(define-scenario
 :name "rainy-summer-day"
 :description "Cloudy summer; shallow belly, modest peaks. Bias swings 0.45 → 0.70."
 :date "2026-07-15"  ;; mid-summer; the clouds, not the season, kill solar
 :stages
 '((:name "00:00 overnight"     :hour-from 0.0  :hour-to 5.0  :bias-from 0.50 :bias-to 0.50
    :cloud-cover 0.85 :mean-wind 6.0 :temperature-base 287.0)
   (:name "05:00 dawn ramp"     :hour-from 5.0  :hour-to 8.0  :bias-from 0.50 :bias-to 0.62
    :cloud-cover 0.85 :mean-wind 6.0 :temperature-base 289.0)
   (:name "08:00 morning peak"  :hour-from 8.0  :hour-to 10.0 :bias-from 0.62 :bias-to 0.58
    :cloud-cover 0.85 :mean-wind 6.5 :temperature-base 291.0)
   (:name "10:00 mild slope"    :hour-from 10.0 :hour-to 13.0 :bias-from 0.58 :bias-to 0.45
    :cloud-cover 0.80 :mean-wind 7.0 :temperature-base 292.0)
   (:name "13:00 mild belly"    :hour-from 13.0 :hour-to 16.0 :bias-from 0.45 :bias-to 0.50
    :cloud-cover 0.80 :mean-wind 7.0 :temperature-base 293.0)
   (:name "16:00 evening ramp"  :hour-from 16.0 :hour-to 18.0 :bias-from 0.50 :bias-to 0.62
    :cloud-cover 0.80 :mean-wind 6.5 :temperature-base 291.0)
   (:name "18:00 evening peak"  :hour-from 18.0 :hour-to 21.0 :bias-from 0.62 :bias-to 0.70
    :cloud-cover 0.85 :mean-wind 6.0 :temperature-base 289.0)
   (:name "21:00 wind-down"     :hour-from 21.0 :hour-to 23.0 :bias-from 0.70 :bias-to 0.58
    :cloud-cover 0.85 :mean-wind 5.5 :temperature-base 287.0)
   (:name "23:00 late night"    :hour-from 23.0 :hour-to 24.0 :bias-from 0.58 :bias-to 0.50
    :cloud-cover 0.85 :mean-wind 5.5 :temperature-base 286.0)))
