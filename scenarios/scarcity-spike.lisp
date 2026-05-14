;; scenarios/scarcity-spike.lisp — synthetic scarcity event. Bias
;; pinned hard buy-side through the evening peak window so MM
;; tilts both bid and ask up by ~12 EUR (with default MM_BIAS_SCALE
;; 25) and follow-last-trade drift pulls the reference 1+ EUR per
;; tick. Within a few minutes of clicking Start during the 17-22h
;; window, prices on the imminent quarters climb several hundred
;; EUR — models a cold-snap-plus-outage night.

(define-scenario
 :name "scarcity-spike"
 :description "Evening peak goes parabolic. Bias pinned at 0.95 between 17:00 and 22:00."
 :stages '(;; name                  hr-from  hr-to  bias-from  bias-to
           ("overnight calm"          0.0     5.0   0.50       0.50)
           ("morning normal"          5.0    10.0   0.55       0.60)
           ("midday quiet"           10.0    15.0   0.50       0.50)
           ("afternoon ramp"         15.0    17.0   0.55       0.85)
           ("evening scarcity"       17.0    22.0   0.95       0.97)
           ("late cooling"           22.0    24.0   0.85       0.55)))
