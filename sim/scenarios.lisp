;; tradingsim scenarios — schedulable market animations on top of
;; (every …) from sim/common.lisp.
;;
;; Each scenario exposes a `scenario-NAME-start` / -stop function
;; pair. Load this file from config.lisp:
;;
;;   (load "sim/scenarios.lisp")
;;
;; then uncomment a `(scenario-NAME-start)` call to enable. The
;; scenarios all assume a market-maker named "de-lu-q0" exists; edit
;; or duplicate-rename for your own MM ids.

;; --- Morning ramp ---------------------------------------------------------
;;
;; Demand on the named MM ramps from 0 to 0.40 EUR/MWh over the
;; first 60 seconds, then sits at the peak. Useful for testing how
;; a trading bot reacts to gradually rising procurement pressure.

(setq scenario-morning-ramp-timer nil)
(setq scenario-morning-ramp-step 0)

(defun scenario-morning-ramp-start (mm-name)
  (setq scenario-morning-ramp-step 0)
  (setq scenario-morning-ramp-timer
        (every
         :milliseconds 5000
         :call (lambda ()
                 (setq scenario-morning-ramp-step
                       (min 12 (+ scenario-morning-ramp-step 1)))
                 (set-mm-demand mm-name
                                (* scenario-morning-ramp-step 0.033))))))

(defun scenario-morning-ramp-stop ()
  (when scenario-morning-ramp-timer
    (cancel-timer scenario-morning-ramp-timer)
    (setq scenario-morning-ramp-timer nil)))

;; --- Gate-closure crunch --------------------------------------------------
;;
;; Spread on the named MM widens stepwise as we get closer to the
;; gate. Real intraday-market behaviour: liquidity thins, spreads widen, MMs
;; cancel resting orders, prints get sparse. The size knob also
;; decreases.

(setq scenario-gate-crunch-timer nil)
(setq scenario-gate-crunch-step 0)

(defun scenario-gate-crunch-start (mm-name)
  (setq scenario-gate-crunch-step 0)
  (setq scenario-gate-crunch-timer
        (every
         :milliseconds 3000
         :call (lambda ()
                 (setq scenario-gate-crunch-step
                       (+ scenario-gate-crunch-step 1))
                 ;; Spread doubles each step until it caps at 4.0.
                 (set-mm-spread mm-name
                                (min 4.0
                                     (* 0.40
                                        (expt 1.5
                                              scenario-gate-crunch-step))))
                 ;; Size shrinks by 20% each step until 0.1 MW floor.
                 (set-mm-size mm-name
                              (max 0.1
                                   (* 1.0
                                      (expt 0.8
                                            scenario-gate-crunch-step))))))))

(defun scenario-gate-crunch-stop ()
  (when scenario-gate-crunch-timer
    (cancel-timer scenario-gate-crunch-timer)
    (setq scenario-gate-crunch-timer nil)))

;; --- Unbalanced fleet (curtailment dump) ----------------------------------
;;
;; A surge of supply: surplus tilts the ask price down by a stepped
;; amount, then holds. Models a sudden curtailment / forced-output
;; event a trading bot needs to absorb.

(setq scenario-curtailment-timer nil)

(defun scenario-curtailment-start (mm-name)
  (setq scenario-curtailment-timer
        (every
         :milliseconds 2000
         :call (lambda ()
                 (set-mm-surplus mm-name 5.0)))))

(defun scenario-curtailment-stop ()
  (when scenario-curtailment-timer
    (cancel-timer scenario-curtailment-timer)
    (setq scenario-curtailment-timer nil)))

;; --- Elaborate 3-hour scenario --------------------------------------------
;;
;; Cycles a single MM + aggressor pair through six 30-minute phases.
;; Exercises every market-shape dial the sim exposes (side-bias,
;; demand, surplus, spread, size, noise, follow-last-trade). Useful
;; as a long-running soak test or as the backdrop for a trading-bot
;; integration run.
;;
;;   0:00 - 0:30  calm baseline (balanced flow, light noise)
;;   0:30 - 1:00  morning ramp (demand rises, bias leans buy)
;;   1:00 - 1:30  mid-day volatility (wider spread, bias flips every 10s)
;;   1:30 - 2:00  curtailment shock (heavy sell-flow, surplus surge)
;;   2:00 - 2:30  recovery (balanced flow returns)
;;   2:30 - 3:00  gate-closure crunch (spread balloons, size shrinks)
;;   3:00         restore baseline + log completion

(setq scenario-elab-timers nil)
(setq scenario-elab-flip-timer nil)

(defun scenario-elaborate-baseline (mm-name ag-name)
  "Reset every MM + aggressor knob the scenario touches to baseline."
  (set-mm-spread mm-name 0.40)
  (set-mm-size mm-name 1.0)
  (set-mm-noise mm-name 0.10)
  (set-mm-demand mm-name 0.0)
  (set-mm-surplus mm-name 0.0)
  (set-mm-follow-last-trade mm-name 0.10)
  (set-aggressor-size ag-name 0.2)
  (set-aggressor-side-bias ag-name 0.5))

(defun scenario-elaborate-start (mm-name ag-name)
  "Run a 3-hour, six-phase market-condition tour against the named
market-maker + aggressor. Cancel mid-run via scenario-elaborate-stop."
  (scenario-elaborate-baseline mm-name ag-name)
  (log.info "scenario-elaborate: phase 1/6 - calm baseline (0:00)")
  (setq scenario-elab-timers
        (list
         ;; Phase 2 — morning ramp
         (run-with-timer
          1800.0 0
          (lambda ()
            (log.info "scenario-elaborate: phase 2/6 - morning ramp (0:30)")
            (set-aggressor-side-bias ag-name 0.75)
            (set-mm-demand mm-name 0.10)))
         ;; Phase 3 — mid-day volatility, random bias flip every 10s
         (run-with-timer
          3600.0 0
          (lambda ()
            (log.info "scenario-elaborate: phase 3/6 - mid-day volatility (1:00)")
            (set-mm-spread mm-name 0.60)
            (set-mm-noise mm-name 0.25)
            (setq scenario-elab-flip-timer
                  (run-with-timer
                   10.0 10.0
                   (lambda ()
                     (set-aggressor-side-bias
                      ag-name
                      (if (> (random 10) 5) 0.70 0.30)))))))
         ;; Phase 4 — curtailment shock
         (run-with-timer
          5400.0 0
          (lambda ()
            (log.info "scenario-elaborate: phase 4/6 - curtailment shock (1:30)")
            (when scenario-elab-flip-timer
              (cancel-timer scenario-elab-flip-timer)
              (setq scenario-elab-flip-timer nil))
            (set-aggressor-side-bias ag-name 0.15)
            (set-aggressor-size ag-name 0.3)
            (set-mm-surplus mm-name 0.30)
            (set-mm-demand mm-name 0.0)))
         ;; Phase 5 — recovery
         (run-with-timer
          7200.0 0
          (lambda ()
            (log.info "scenario-elaborate: phase 5/6 - recovery (2:00)")
            (set-aggressor-side-bias ag-name 0.55)
            (set-aggressor-size ag-name 0.2)
            (set-mm-surplus mm-name 0.0)
            (set-mm-spread mm-name 0.40)
            (set-mm-noise mm-name 0.10)))
         ;; Phase 6 — gate-closure crunch
         (run-with-timer
          9000.0 0
          (lambda ()
            (log.info "scenario-elaborate: phase 6/6 - gate-closure crunch (2:30)")
            (set-mm-spread mm-name 1.20)
            (set-mm-size mm-name 0.3)
            (set-mm-noise mm-name 0.40)
            (set-aggressor-size ag-name 0.1)))
         ;; End — restore baseline
         (run-with-timer
          10800.0 0
          (lambda ()
            (log.info "scenario-elaborate: complete (3:00) - restoring baseline")
            (scenario-elaborate-baseline mm-name ag-name))))))

(defun scenario-elaborate-stop ()
  "Cancel every timer the elaborate scenario is holding."
  (dolist (tm scenario-elab-timers)
    (cancel-timer tm))
  (when scenario-elab-flip-timer
    (cancel-timer scenario-elab-flip-timer)
    (setq scenario-elab-flip-timer nil))
  (setq scenario-elab-timers nil)
  (log.info "scenario-elaborate: stopped"))
