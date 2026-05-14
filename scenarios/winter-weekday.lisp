;; scenarios/winter-weekday.lisp — heating-driven double peak, very
;; weak solar. No belly worth the name; bias stays buy-heavy almost
;; all day, with sharp morning and evening peaks.

(define-scenario
 :name "winter-weekday"
 :description "Heating-driven double peak, weak solar. Bias hovers 0.55 → 0.82."
 :date "2026-01-15"  ;; deep January — short days, low sun
 :stages
 '((:name "overnight"     :hour-from 0.0  :hour-to 5.0  :bias-from 0.52 :bias-to 0.52
    :cloud-cover 0.70 :mean-wind 5.0 :temperature-base 270.0)
   (:name "dawn ramp"     :hour-from 5.0  :hour-to 8.0  :bias-from 0.52 :bias-to 0.78
    :cloud-cover 0.70 :mean-wind 5.0 :temperature-base 270.0)
   (:name "morning peak"  :hour-from 8.0  :hour-to 10.0 :bias-from 0.78 :bias-to 0.65
    :cloud-cover 0.65 :mean-wind 5.0 :temperature-base 272.0)
   (:name "plateau"       :hour-from 10.0 :hour-to 13.0 :bias-from 0.65 :bias-to 0.55
    :cloud-cover 0.60 :mean-wind 5.0 :temperature-base 274.0)
   (:name "plateau"       :hour-from 13.0 :hour-to 16.0 :bias-from 0.55 :bias-to 0.55
    :cloud-cover 0.60 :mean-wind 5.0 :temperature-base 275.0)
   (:name "evening ramp"  :hour-from 16.0 :hour-to 18.0 :bias-from 0.55 :bias-to 0.75
    :cloud-cover 0.65 :mean-wind 5.0 :temperature-base 273.0)
   (:name "evening peak"  :hour-from 18.0 :hour-to 21.0 :bias-from 0.75 :bias-to 0.82
    :cloud-cover 0.70 :mean-wind 5.5 :temperature-base 271.0)
   (:name "wind-down"     :hour-from 21.0 :hour-to 23.0 :bias-from 0.82 :bias-to 0.62
    :cloud-cover 0.70 :mean-wind 5.5 :temperature-base 269.0)
   (:name "late night"    :hour-from 23.0 :hour-to 24.0 :bias-from 0.62 :bias-to 0.52
    :cloud-cover 0.75 :mean-wind 5.0 :temperature-base 268.0)))
