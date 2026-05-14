;; scenarios/rainy-summer-day.lisp — clouds dampen solar production,
;; so the midday belly stays shallow and the evening peak is modest.
;; Useful as the "what does the curve look like when the weather
;; misbehaves" foil for the sunny-summer scenarios.

(define-scenario
 :name "rainy-summer-day"
 :description "Cloudy summer; shallow belly, modest peaks. Bias swings 0.45 → 0.70."
 :stages '(("00:00 overnight"      0.0  5.0  0.50 0.50)
           ("05:00 dawn ramp"      5.0  8.0  0.50 0.62)
           ("08:00 morning peak"   8.0 10.0  0.62 0.58)
           ("10:00 mild slope"    10.0 13.0  0.58 0.45)
           ("13:00 mild belly"    13.0 16.0  0.45 0.50)
           ("16:00 evening ramp"  16.0 18.0  0.50 0.62)
           ("18:00 evening peak"  18.0 21.0  0.62 0.70)
           ("21:00 wind-down"     21.0 23.0  0.70 0.58)
           ("23:00 late night"    23.0 24.0  0.58 0.50)))
