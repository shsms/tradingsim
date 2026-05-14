;; Sample tradingsim config — loaded by the binary at startup.
;;
;; This file replaces the previous hardcoded defaults in
;; bin/tradingsim.rs. Edit and re-launch the binary to take effect;
;; hot reload (notify watcher) is on the deferred list.

(unless (boundp 'tradingsim-loaded)
  (setq tradingsim-loaded t)
  (load "sim/common.lisp"))

;; Cancel any timers from a previous load before the new config
;; re-registers them. Required for hot reload to start clean.
(reset-state)

;; Watch the support files so saving them also triggers a reload.
(watch-file "sim/common.lisp")

(set-socket-addr "[::1]:8810")
(set-physics-tick-ms 100)

;; Markets — one per delivery area, all four durations (5/15/30/60)
;; admissible by default. Add more (%make-market …) entries for FR /
;; AT / NL / BE if you want multi-area gridpools.
(%make-market
 :area "10Y1001A1001A82H"
 :currency "eur")

;; One gridpool trading in DE-LU.
(%make-gridpool
 :id 1
 :name "default"
 :areas '("10Y1001A1001A82H"))

;; Fleet of 48 MMs — one per 15-min contract spanning the next 12
;; hours of delivery. Each MM's quarter_offset drives the rolling
;; task in the binary: as a contract gates out, the MM that quoted
;; it migrates to the next-12h slot. Names are stable ("de-lu-q0"
;; … "de-lu-q47") so scenarios can still address individual MMs.

(dotimes (i 48)
  (%make-market-maker
   :name (format "de-lu-q%d" i)
   :area "10Y1001A1001A82H"
   :quarter-offset i
   :reference 85.00
   :spread 0.40
   :size 1.0
   :noise 0.10
   :seed (+ 1 i)))

;; --- Aggressors ------------------------------------------------------------
;;
;; Four aggressors per quarter (192 total) so every contract has
;; some live flow on its tape. Rate scales linearly with the
;; quarter-offset: q0's aggressors fire every 500 ms, q47's every
;; 24 s — imminent contracts trade much harder than the
;; long-dated ones, matching real intraday volume profiles.

(dotimes (q 48)
  (dotimes (a 4)
    (%make-aggressor
     :name (format "ag-q%d-%d" q a)
     :area "10Y1001A1001A82H"
     :quarter-offset q
     :rate-ms (* 500 (+ q 1))
     :size 0.2
     :side-bias 0.5
     :seed (+ 1000 (* q 10) a))))

;; --- Reference drift -------------------------------------------------------
;;
;; Tie every MM's reference to the last public trade on its contract
;; so prices migrate with activity. 0.10 = 10% pull each refresh.

(dotimes (i 48)
  (set-mm-follow-last-trade (format "de-lu-q%d" i) 0.10))

;; --- Demand / surplus tilts ------------------------------------------------
;;
;; demand shifts the bid up (the MM wants to buy harder); surplus
;; shifts the ask down (the MM wants to sell harder). Uncomment to
;; bias the simulated market.
;;
;; (set-mm-demand "de-lu-q2" 0.20)   ;; peak quarter: aggressive procurement
;; (set-mm-surplus "de-lu-q3" 0.30)  ;; midday solar dump

;; --- Scenarios -------------------------------------------------------------
;;
;; Each script in scenarios/ is self-running on load. Uncomment any
;; line below to activate the matching market animation; each
;; scenario exposes a (scenario-NAME-stop) defun for manual cancel.
;;
;; (load "scenarios/morning-ramp.lisp")   ;; demand ramp on de-lu-q0
;; (load "scenarios/gate-crunch.lisp")    ;; widening spread on de-lu-q3
;; (load "scenarios/curtailment.lisp")    ;; supply surge on de-lu-q2
;; (load "scenarios/elaborate.lisp")      ;; 3-hour six-phase tour
