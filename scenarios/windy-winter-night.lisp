;; scenarios/windy-winter-night.lisp — wind production runs hot
;; overnight and into the morning. Surplus tilts bias sell-heavy at
;; night; daytime curves stay flat and modest.

(define-scenario
 :name "windy-winter-night"
 :description "Wind surplus overnight; modest daytime swings. Bias 0.42 → 0.62."
 :stages '(("00:00 overnight"      0.0  5.0  0.42 0.42)
           ("05:00 dawn ramp"      5.0  8.0  0.42 0.55)
           ("08:00 morning peak"   8.0 10.0  0.55 0.50)
           ("10:00 plateau"       10.0 13.0  0.50 0.45)
           ("13:00 plateau"       13.0 16.0  0.45 0.45)
           ("16:00 evening ramp"  16.0 18.0  0.45 0.55)
           ("18:00 evening peak"  18.0 21.0  0.55 0.62)
           ("21:00 wind-down"     21.0 23.0  0.62 0.48)
           ("23:00 late night"    23.0 24.0  0.48 0.42)))
