;; scenarios/duck-curve.lisp — drive the trade tape with a German
;; intraday-style demand/supply curve. The scenario doesn't write
;; reference prices directly: it skews each aggressor's side-bias
;; from the time-of-day of its target contract, so flow goes
;; sell-heavy through the solar belly and buy-heavy in the evening
;; peak. Prices emerge from those imbalances via the MM's
;; follow-last-trade drift.
;;
;; Stage:
;;   "duck curve active" — sets a 30 s timer that recomputes every
;;                         aggressor's bias from its delivery hour.
;;
;; on-stop:
;;   Cancels the timer and resets every aggressor to side-bias 0.5.

(load "sim/common.lisp")

(setq duck-prefixes '("tn" "am" "hz" "bw"))
(setq duck-quarters 48)
(setq duck-profiles 4)

(unless (boundp 'duck-timer)
  (setq duck-timer nil))

(defun duck-bias-for-hour (h)
  "Side-bias for a given UTC hour-of-day. <0.5 = sell-heavy
(drives prices down), >0.5 = buy-heavy (drives prices up)."
  (cond
    ((< h 6)  0.50)    ;; overnight: balanced
    ((< h 9)  0.62)    ;; morning ramp: buy demand
    ((< h 10) 0.55)
    ((< h 15) 0.35)    ;; solar belly: PV operators dumping
    ((< h 17) 0.50)    ;; transition
    ((< h 21) 0.72)    ;; evening peak: tight supply
    ((< h 23) 0.60)
    (t        0.50)))  ;; late night: balanced

(defun duck-tick ()
  "Walk every aggressor and set its side-bias from its target
period's hour-of-day."
  (dotimes (i duck-quarters)
    (let ((bias (duck-bias-for-hour (quarter-offset-hour i))))
      (dolist (prefix duck-prefixes)
        (dotimes (p duck-profiles)
          (set-aggressor-side-bias
           (format "ag-%s-q%d-%d" prefix i p)
           bias))))))

(defun duck-start ()
  "Apply the curve once and schedule a 30-second refresh."
  (when duck-timer
    (cancel-timer duck-timer)
    (setq duck-timer nil))
  (duck-tick)
  (setq duck-timer (run-with-timer 30.0 30.0 'duck-tick)))

(defun duck-stop ()
  "Cancel the refresh timer and reset every aggressor to balanced."
  (when duck-timer
    (cancel-timer duck-timer)
    (setq duck-timer nil))
  (dolist (prefix duck-prefixes)
    (dotimes (i duck-quarters)
      (dotimes (p duck-profiles)
        (set-aggressor-side-bias
         (format "ag-%s-q%d-%d" prefix i p) 0.5)))))

(define-scenario
 :name "duck-curve"
 :description "Demand/supply imbalance follows the German intraday duck curve — prices drift down through the solar belly, up into the evening peak."
 :stages '(("duck curve active" "duck-start"))
 :on-stop "duck-stop")
