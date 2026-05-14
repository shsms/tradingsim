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
 :stages
 '((:name "overnight calm"    :hour-from 0.0  :hour-to 5.0  :bias-from 0.50 :bias-to 0.50)
   (:name "morning normal"    :hour-from 5.0  :hour-to 10.0 :bias-from 0.55 :bias-to 0.60)
   (:name "midday quiet"      :hour-from 10.0 :hour-to 15.0 :bias-from 0.50 :bias-to 0.50)
   (:name "afternoon ramp"    :hour-from 15.0 :hour-to 17.0 :bias-from 0.55 :bias-to 0.85)
   (:name "evening scarcity"  :hour-from 17.0 :hour-to 22.0 :bias-from 0.95 :bias-to 0.97)
   (:name "late cooling"      :hour-from 22.0 :hour-to 24.0 :bias-from 0.85 :bias-to 0.55)))
