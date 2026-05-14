;; scenarios/windy-winter-night.lisp — wind production runs hot
;; overnight and into the morning. Surplus tilts bias sell-heavy at
;; night; daytime curves stay flat and modest.

(define-scenario
 :name "windy-winter-night"
 :description "Wind surplus overnight; modest daytime swings. Bias 0.42 → 0.62."
 :stages
 '((:name "00:00 overnight"     :hour-from 0.0  :hour-to 5.0  :bias-from 0.42 :bias-to 0.42)
   (:name "05:00 dawn ramp"     :hour-from 5.0  :hour-to 8.0  :bias-from 0.42 :bias-to 0.55)
   (:name "08:00 morning peak"  :hour-from 8.0  :hour-to 10.0 :bias-from 0.55 :bias-to 0.50)
   (:name "10:00 plateau"       :hour-from 10.0 :hour-to 13.0 :bias-from 0.50 :bias-to 0.45)
   (:name "13:00 plateau"       :hour-from 13.0 :hour-to 16.0 :bias-from 0.45 :bias-to 0.45)
   (:name "16:00 evening ramp"  :hour-from 16.0 :hour-to 18.0 :bias-from 0.45 :bias-to 0.55)
   (:name "18:00 evening peak"  :hour-from 18.0 :hour-to 21.0 :bias-from 0.55 :bias-to 0.62)
   (:name "21:00 wind-down"     :hour-from 21.0 :hour-to 23.0 :bias-from 0.62 :bias-to 0.48)
   (:name "23:00 late night"    :hour-from 23.0 :hour-to 24.0 :bias-from 0.48 :bias-to 0.42)))
