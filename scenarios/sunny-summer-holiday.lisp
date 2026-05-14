;; scenarios/sunny-summer-holiday.lisp — strong PV plus suppressed
;; demand (industry idle, people away). Deepest belly of the set,
;; muted evening peak.

(define-scenario
 :name "sunny-summer-holiday"
 :description "PV at full pour, demand suppressed. Bias swings 0.18 → 0.58."
 :stages '(;; name                  hr-from  hr-to  bias-from  bias-to
           ("00:00 overnight"        0.0     5.0   0.48       0.48)
           ("05:00 dawn ramp"        5.0     8.0   0.48       0.55)
           ("08:00 morning peak"     8.0    10.0   0.55       0.45)
           ("10:00 belly slope"     10.0    13.0   0.45       0.18)
           ("13:00 deep belly"      13.0    16.0   0.18       0.25)
           ("16:00 evening ramp"    16.0    18.0   0.25       0.50)
           ("18:00 evening peak"    18.0    21.0   0.50       0.58)
           ("21:00 wind-down"       21.0    23.0   0.58       0.50)
           ("23:00 late night"      23.0    24.0   0.50       0.48)))
