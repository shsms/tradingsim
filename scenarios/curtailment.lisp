;; scenarios/curtailment.lisp — sudden supply surge on de-lu-q2.
;; Holds surplus at 5.0 EUR/MWh to model a forced-output /
;; curtailment event a trading bot needs to absorb.
;;
;; Load from config.lisp or the REPL:
;;
;;   (load "scenarios/curtailment.lisp")
;;
;; Cancel mid-run with (scenario-curtailment-stop).

(load "sim/common.lisp")

(defun scenario-curtailment-stop ()
  (when (and (boundp 'scenario-curtailment-timer)
             scenario-curtailment-timer)
    (cancel-timer scenario-curtailment-timer)
    (setq scenario-curtailment-timer nil)))

(scenario-curtailment-stop)

(setq scenario-curtailment-timer
      (every
       :milliseconds 2000
       :call (lambda ()
               (set-mm-surplus "de-lu-q2" 5.0))))

(log.info "scenario-curtailment: armed against de-lu-q2")
