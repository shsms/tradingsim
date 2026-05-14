;; scenarios/sunny-summer-day.lisp — German summer day with strong
;; solar. Deep belly around midday (PV operators dumping into a
;; saturated market), sharp evening ramp as the sun sets, peak
;; demand 18:00-21:00.

(define-scenario
 :name "sunny-summer-day"
 :description "Strong solar belly, sharp evening peak. Bias swings 0.20 → 0.78."
 :stages
 '((:name "00:00 overnight"     :hour-from 0.0  :hour-to 5.0  :bias-from 0.50 :bias-to 0.50)
   (:name "05:00 dawn ramp"     :hour-from 5.0  :hour-to 8.0  :bias-from 0.50 :bias-to 0.65)
   (:name "08:00 morning peak"  :hour-from 8.0  :hour-to 10.0 :bias-from 0.65 :bias-to 0.55)
   (:name "10:00 belly slope"   :hour-from 10.0 :hour-to 13.0 :bias-from 0.55 :bias-to 0.20)
   (:name "13:00 deep belly"    :hour-from 13.0 :hour-to 16.0 :bias-from 0.20 :bias-to 0.30)
   (:name "16:00 evening ramp"  :hour-from 16.0 :hour-to 18.0 :bias-from 0.30 :bias-to 0.65)
   (:name "18:00 evening peak"  :hour-from 18.0 :hour-to 21.0 :bias-from 0.65 :bias-to 0.78)
   (:name "21:00 wind-down"     :hour-from 21.0 :hour-to 23.0 :bias-from 0.78 :bias-to 0.60)
   (:name "23:00 late night"    :hour-from 23.0 :hour-to 24.0 :bias-from 0.60 :bias-to 0.50)))
