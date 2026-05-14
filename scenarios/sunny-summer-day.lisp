;; scenarios/sunny-summer-day.lisp — German summer day with strong
;; solar. Deep belly around midday (PV operators dumping into a
;; saturated market), sharp evening ramp as the sun sets, peak
;; demand 18:00-21:00.

(define-scenario
 :name "sunny-summer-day"
 :description "Strong solar belly, sharp evening peak. Bias swings 0.20 → 0.78."
 :stages '(;; name                  hr-from  hr-to  bias-from  bias-to
           ("00:00 overnight"        0.0     5.0   0.50       0.50)
           ("05:00 dawn ramp"        5.0     8.0   0.50       0.65)
           ("08:00 morning peak"     8.0    10.0   0.65       0.55)
           ("10:00 belly slope"     10.0    13.0   0.55       0.20)
           ("13:00 deep belly"      13.0    16.0   0.20       0.30)
           ("16:00 evening ramp"    16.0    18.0   0.30       0.65)
           ("18:00 evening peak"    18.0    21.0   0.65       0.78)
           ("21:00 wind-down"       21.0    23.0   0.78       0.60)
           ("23:00 late night"      23.0    24.0   0.60       0.50)))
