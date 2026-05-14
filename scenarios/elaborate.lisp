;; scenarios/elaborate.lisp — 3-hour six-phase market tour.
;;
;; Cycles de-lu-q0 + ag-q0 through six 30-minute phases, exercising
;; every market-shape dial the sim exposes (side-bias, demand,
;; surplus, spread, size, noise, follow-last-trade). Useful as a
;; long-running soak test or the backdrop for a trading-bot
;; integration run.
;;
;;   0:00 - 0:30  calm baseline (balanced flow, light noise)
;;   0:30 - 1:00  morning ramp (demand rises, bias leans buy)
;;   1:00 - 1:30  mid-day volatility (wider spread, bias flips every 10s)
;;   1:30 - 2:00  curtailment shock (heavy sell-flow, surplus surge)
;;   2:00 - 2:30  recovery (balanced flow returns)
;;   2:30 - 3:00  gate-closure crunch (spread balloons, size shrinks)
;;   3:00         restore baseline + log completion
;;
;; Load from config.lisp or the REPL:
;;
;;   (load "scenarios/elaborate.lisp")
;;
;; Cancel mid-run with (scenario-elaborate-stop).

(load "sim/common.lisp")

(setq scenario-elaborate-flip-timer nil)

(defun scenario-elaborate-baseline ()
  "Reset every MM + aggressor knob the scenario touches to baseline."
  (set-mm-spread "de-lu-q0" 0.40)
  (set-mm-size "de-lu-q0" 1.0)
  (set-mm-noise "de-lu-q0" 0.10)
  (set-mm-demand "de-lu-q0" 0.0)
  (set-mm-surplus "de-lu-q0" 0.0)
  (set-mm-follow-last-trade "de-lu-q0" 0.10)
  (set-aggressor-size "ag-q0" 0.2)
  (set-aggressor-side-bias "ag-q0" 0.5))

(defun scenario-elaborate-stop ()
  (when (boundp 'scenario-elaborate-timers)
    (dolist (tm scenario-elaborate-timers)
      (cancel-timer tm)))
  (when scenario-elaborate-flip-timer
    (cancel-timer scenario-elaborate-flip-timer)
    (setq scenario-elaborate-flip-timer nil))
  (setq scenario-elaborate-timers nil))

(scenario-elaborate-stop)

(scenario-elaborate-baseline)
(log.info "scenario-elaborate: phase 1/6 - calm baseline (0:00)")

(setq scenario-elaborate-timers
      (list
       ;; Phase 2 — morning ramp
       (run-with-timer
        1800.0 0
        (lambda ()
          (log.info "scenario-elaborate: phase 2/6 - morning ramp (0:30)")
          (set-aggressor-side-bias "ag-q0" 0.75)
          (set-mm-demand "de-lu-q0" 0.10)))
       ;; Phase 3 — mid-day volatility, random bias flip every 10s
       (run-with-timer
        3600.0 0
        (lambda ()
          (log.info "scenario-elaborate: phase 3/6 - mid-day volatility (1:00)")
          (set-mm-spread "de-lu-q0" 0.60)
          (set-mm-noise "de-lu-q0" 0.25)
          (setq scenario-elaborate-flip-timer
                (run-with-timer
                 10.0 10.0
                 (lambda ()
                   (set-aggressor-side-bias
                    "ag-q0"
                    (if (> (random 10) 5) 0.70 0.30)))))))
       ;; Phase 4 — curtailment shock
       (run-with-timer
        5400.0 0
        (lambda ()
          (log.info "scenario-elaborate: phase 4/6 - curtailment shock (1:30)")
          (when scenario-elaborate-flip-timer
            (cancel-timer scenario-elaborate-flip-timer)
            (setq scenario-elaborate-flip-timer nil))
          (set-aggressor-side-bias "ag-q0" 0.15)
          (set-aggressor-size "ag-q0" 0.3)
          (set-mm-surplus "de-lu-q0" 0.30)
          (set-mm-demand "de-lu-q0" 0.0)))
       ;; Phase 5 — recovery
       (run-with-timer
        7200.0 0
        (lambda ()
          (log.info "scenario-elaborate: phase 5/6 - recovery (2:00)")
          (set-aggressor-side-bias "ag-q0" 0.55)
          (set-aggressor-size "ag-q0" 0.2)
          (set-mm-surplus "de-lu-q0" 0.0)
          (set-mm-spread "de-lu-q0" 0.40)
          (set-mm-noise "de-lu-q0" 0.10)))
       ;; Phase 6 — gate-closure crunch
       (run-with-timer
        9000.0 0
        (lambda ()
          (log.info "scenario-elaborate: phase 6/6 - gate-closure crunch (2:30)")
          (set-mm-spread "de-lu-q0" 1.20)
          (set-mm-size "de-lu-q0" 0.3)
          (set-mm-noise "de-lu-q0" 0.40)
          (set-aggressor-size "ag-q0" 0.1)))
       ;; End — restore baseline
       (run-with-timer
        10800.0 0
        (lambda ()
          (log.info "scenario-elaborate: complete (3:00) - restoring baseline")
          (scenario-elaborate-baseline)))))

(log.info "scenario-elaborate: armed against de-lu-q0 + ag-q0 (3h)")
