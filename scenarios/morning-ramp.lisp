;; scenarios/morning-ramp.lisp — demand on de-lu-q0 ramps from 0 to
;; 0.40 EUR/MWh over the first 60 seconds, then sits at the peak.
;; Useful for testing how a trading bot reacts to gradually rising
;; procurement pressure.
;;
;; Load from config.lisp or the REPL:
;;
;;   (load "scenarios/morning-ramp.lisp")
;;
;; Cancel mid-run with (scenario-morning-ramp-stop).

(load "sim/common.lisp")

(setq scenario-morning-ramp-step 0)

(defun scenario-morning-ramp-stop ()
  (when (and (boundp 'scenario-morning-ramp-timer)
             scenario-morning-ramp-timer)
    (cancel-timer scenario-morning-ramp-timer)
    (setq scenario-morning-ramp-timer nil)))

(scenario-morning-ramp-stop)

(setq scenario-morning-ramp-step 0)
(setq scenario-morning-ramp-timer
      (every
       :milliseconds 5000
       :call (lambda ()
               (setq scenario-morning-ramp-step
                     (min 12 (+ scenario-morning-ramp-step 1)))
               (set-mm-demand "de-lu-q0"
                              (* scenario-morning-ramp-step 0.033)))))

(log.info "scenario-morning-ramp: armed against de-lu-q0")
