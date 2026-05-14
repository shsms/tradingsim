;; scenarios/winter-weekday.lisp — heating-driven double peak, very
;; weak solar. No belly worth the name; bias stays buy-heavy almost
;; all day, with sharp morning and evening peaks.

(define-scenario
 :name "winter-weekday"
 :description "Heating-driven double peak, weak solar. Bias hovers 0.55 → 0.82."
 :stages '(;; name                  hr-from  hr-to  bias-from  bias-to
           ("00:00 overnight"        0.0     5.0   0.52       0.52)
           ("05:00 dawn ramp"        5.0     8.0   0.52       0.78)
           ("08:00 morning peak"     8.0    10.0   0.78       0.65)
           ("10:00 plateau"         10.0    13.0   0.65       0.55)
           ("13:00 plateau"         13.0    16.0   0.55       0.55)
           ("16:00 evening ramp"    16.0    18.0   0.55       0.75)
           ("18:00 evening peak"    18.0    21.0   0.75       0.82)
           ("21:00 wind-down"       21.0    23.0   0.82       0.62)
           ("23:00 late night"      23.0    24.0   0.62       0.52)))
