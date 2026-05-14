;; scenarios/sunny-summer-day.lisp — German summer day with strong
;; solar. Deep belly around midday (PV operators dumping into a
;; saturated market), sharp evening ramp as the sun sets, peak
;; demand 18:00-21:00.

(define-scenario
 :name "sunny-summer-day"
 :description "Strong solar belly, sharp evening peak. Bias swings 0.20 → 0.78."
 :date "2026-06-21"  ;; summer solstice — high sun, long daylight
 :stages
 '((:name "overnight"     :hour-from 0.0  :hour-to 5.0  :bias-from 0.50 :bias-to 0.50
    :cloud-cover 0.10 :mean-wind 3.0 :temperature-base 287.0)
   (:name "dawn ramp"     :hour-from 5.0  :hour-to 8.0  :bias-from 0.50 :bias-to 0.65
    :cloud-cover 0.10 :mean-wind 3.5 :temperature-base 290.0)
   (:name "morning peak"  :hour-from 8.0  :hour-to 10.0 :bias-from 0.65 :bias-to 0.55
    :cloud-cover 0.10 :mean-wind 4.0 :temperature-base 294.0)
   (:name "belly slope"   :hour-from 10.0 :hour-to 13.0 :bias-from 0.55 :bias-to 0.20
    :cloud-cover 0.10 :mean-wind 4.5 :temperature-base 298.0)
   (:name "deep belly"    :hour-from 13.0 :hour-to 16.0 :bias-from 0.20 :bias-to 0.30
    :cloud-cover 0.10 :mean-wind 5.0 :temperature-base 301.0)
   (:name "evening ramp"  :hour-from 16.0 :hour-to 18.0 :bias-from 0.30 :bias-to 0.65
    :cloud-cover 0.15 :mean-wind 4.5 :temperature-base 298.0)
   (:name "evening peak"  :hour-from 18.0 :hour-to 21.0 :bias-from 0.65 :bias-to 0.78
    :cloud-cover 0.20 :mean-wind 4.0 :temperature-base 294.0)
   (:name "wind-down"     :hour-from 21.0 :hour-to 23.0 :bias-from 0.78 :bias-to 0.60
    :cloud-cover 0.20 :mean-wind 3.5 :temperature-base 291.0)
   (:name "late night"    :hour-from 23.0 :hour-to 24.0 :bias-from 0.60 :bias-to 0.50
    :cloud-cover 0.20 :mean-wind 3.0 :temperature-base 289.0)))
