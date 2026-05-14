;; scenarios/sunny-summer-holiday.lisp — strong PV plus suppressed
;; demand (industry idle, people away). Deepest belly of the set,
;; muted evening peak.

(define-scenario
 :name "sunny-summer-holiday"
 :description "PV at full pour, demand suppressed. Bias swings 0.18 → 0.58."
 :date "2026-08-15"  ;; August Assumption holiday — sun still high, plants idle
 :stages
 '((:name "00:00 overnight"     :hour-from 0.0  :hour-to 5.0  :bias-from 0.48 :bias-to 0.48
    :cloud-cover 0.05 :mean-wind 2.5 :temperature-base 289.0)
   (:name "05:00 dawn ramp"     :hour-from 5.0  :hour-to 8.0  :bias-from 0.48 :bias-to 0.55
    :cloud-cover 0.05 :mean-wind 3.0 :temperature-base 292.0)
   (:name "08:00 morning peak"  :hour-from 8.0  :hour-to 10.0 :bias-from 0.55 :bias-to 0.45
    :cloud-cover 0.05 :mean-wind 3.5 :temperature-base 296.0)
   (:name "10:00 belly slope"   :hour-from 10.0 :hour-to 13.0 :bias-from 0.45 :bias-to 0.18
    :cloud-cover 0.05 :mean-wind 4.0 :temperature-base 300.0)
   (:name "13:00 deep belly"    :hour-from 13.0 :hour-to 16.0 :bias-from 0.18 :bias-to 0.25
    :cloud-cover 0.05 :mean-wind 4.5 :temperature-base 303.0)
   (:name "16:00 evening ramp"  :hour-from 16.0 :hour-to 18.0 :bias-from 0.25 :bias-to 0.50
    :cloud-cover 0.10 :mean-wind 4.0 :temperature-base 300.0)
   (:name "18:00 evening peak"  :hour-from 18.0 :hour-to 21.0 :bias-from 0.50 :bias-to 0.58
    :cloud-cover 0.10 :mean-wind 3.5 :temperature-base 296.0)
   (:name "21:00 wind-down"     :hour-from 21.0 :hour-to 23.0 :bias-from 0.58 :bias-to 0.50
    :cloud-cover 0.10 :mean-wind 3.0 :temperature-base 293.0)
   (:name "23:00 late night"    :hour-from 23.0 :hour-to 24.0 :bias-from 0.50 :bias-to 0.48
    :cloud-cover 0.10 :mean-wind 2.5 :temperature-base 291.0)))
