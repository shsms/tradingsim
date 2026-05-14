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
