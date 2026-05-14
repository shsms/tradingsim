;; scenarios/elaborate.lisp — six-phase market-condition tour, driven
;; from the UI. Each stage is a plain defun the UI's scenario panel
;; invokes when the operator clicks Start / Next. Stops auto-advance
;; entirely; the previous timer-driven version lives in git history
;; if you want the auto-advance behaviour back.
;;
;; Stages cycle TenneT q0 + the q0 fast aggressor through the
;; canonical intraday narrative:
;;
;;   1. calm baseline       balanced flow, light noise
;;   2. morning ramp        demand rises, bias leans buy
;;   3. mid-day volatility  wider spread, big bias swings
;;   4. curtailment shock   heavy sell-flow, surplus surge
;;   5. recovery            balanced flow returns
;;   6. gate-closure crunch spread balloons, size shrinks
;;
;; Loading the file registers the scenario; the UI doesn't auto-start.

(load "sim/common.lisp")

(defun elab-baseline ()
  (set-mm-spread "tn-q0" 0.40)
  (set-mm-size "tn-q0" 1.0)
  (set-mm-noise "tn-q0" 0.10)
  (set-mm-demand "tn-q0" 0.0)
  (set-mm-surplus "tn-q0" 0.0)
  (set-mm-follow-last-trade "tn-q0" 0.10)
  (set-aggressor-size "ag-tn-q0-0" 0.3)
  (set-aggressor-side-bias "ag-tn-q0-0" 0.5))

(defun elab-ramp ()
  (set-aggressor-side-bias "ag-tn-q0-0" 0.75)
  (set-mm-demand "tn-q0" 0.10))

(defun elab-volatility ()
  (set-mm-spread "tn-q0" 0.60)
  (set-mm-noise "tn-q0" 0.25)
  (set-aggressor-side-bias "ag-tn-q0-0" 0.30))

(defun elab-curtailment ()
  (set-aggressor-side-bias "ag-tn-q0-0" 0.15)
  (set-aggressor-size "ag-tn-q0-0" 0.5)
  (set-mm-surplus "tn-q0" 0.30)
  (set-mm-demand "tn-q0" 0.0))

(defun elab-recovery ()
  (set-aggressor-side-bias "ag-tn-q0-0" 0.55)
  (set-aggressor-size "ag-tn-q0-0" 0.3)
  (set-mm-surplus "tn-q0" 0.0)
  (set-mm-spread "tn-q0" 0.40)
  (set-mm-noise "tn-q0" 0.10))

(defun elab-gate-crunch ()
  (set-mm-spread "tn-q0" 1.20)
  (set-mm-size "tn-q0" 0.3)
  (set-mm-noise "tn-q0" 0.40)
  (set-aggressor-size "ag-tn-q0-0" 0.1))

(define-scenario
 :name "elaborate"
 :description "Six-phase tour through every market-shape dial — manual advance from the UI."
 :stages '(("calm baseline"       "elab-baseline")
           ("morning ramp"        "elab-ramp")
           ("mid-day volatility"  "elab-volatility")
           ("curtailment shock"   "elab-curtailment")
           ("recovery"            "elab-recovery")
           ("gate-closure crunch" "elab-gate-crunch")))
