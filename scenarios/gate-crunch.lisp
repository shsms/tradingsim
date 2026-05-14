;; scenarios/gate-crunch.lisp — spread on de-lu-q3 widens stepwise
;; while size shrinks. Real intraday behaviour: liquidity thins,
;; spreads widen, MMs cancel resting orders, prints get sparse.
;;
;; Load from config.lisp or the REPL:
;;
;;   (load "scenarios/gate-crunch.lisp")
;;
;; Cancel mid-run with (scenario-gate-crunch-stop).

(load "sim/common.lisp")

(setq scenario-gate-crunch-step 0)

(defun scenario-gate-crunch-stop ()
  (when (and (boundp 'scenario-gate-crunch-timer)
             scenario-gate-crunch-timer)
    (cancel-timer scenario-gate-crunch-timer)
    (setq scenario-gate-crunch-timer nil)))

(scenario-gate-crunch-stop)

(setq scenario-gate-crunch-step 0)
(setq scenario-gate-crunch-timer
      (every
       :milliseconds 3000
       :call (lambda ()
               (setq scenario-gate-crunch-step
                     (+ scenario-gate-crunch-step 1))
               ;; Spread climbs by 1.5x each step until it caps at 4.0.
               (set-mm-spread "de-lu-q3"
                              (min 4.0
                                   (* 0.40
                                      (expt 1.5
                                            scenario-gate-crunch-step))))
               ;; Size shrinks by 20% each step until 0.1 MW floor.
               (set-mm-size "de-lu-q3"
                            (max 0.1
                                 (* 1.0
                                    (expt 0.8
                                          scenario-gate-crunch-step)))))))

(log.info "scenario-gate-crunch: armed against de-lu-q3")
