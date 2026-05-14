;; Sample tradingsim config — loaded by the binary at startup.
;;
;; This file replaces the previous hardcoded defaults in
;; bin/tradingsim.rs. Edit and re-launch the binary to take effect;
;; hot reload (notify watcher) is on the deferred list.

(unless (boundp 'tradingsim-loaded)
  (setq tradingsim-loaded t)
  (load "sim/common.lisp"))

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

;; Eight 15-min contracts of synthetic liquidity in DE-LU, starting
;; at the next 15-min boundary. Each MM holds a SharedConfig the
;; (set-mm-* …) defuns mutate in place.

(%make-market-maker
 :name "de-lu-q0"
 :area "10Y1001A1001A82H"
 :quarter-offset 0
 :reference 85.00
 :spread 0.40
 :size 1.0
 :noise 0.10
 :seed 1)

(%make-market-maker
 :name "de-lu-q1"
 :area "10Y1001A1001A82H"
 :quarter-offset 1
 :reference 85.50
 :spread 0.40
 :size 1.0
 :noise 0.10
 :seed 2)

(%make-market-maker
 :name "de-lu-q2"
 :area "10Y1001A1001A82H"
 :quarter-offset 2
 :reference 86.00          ;; ramp into peak quarter
 :spread 0.50
 :size 1.0
 :noise 0.15
 :seed 3)

(%make-market-maker
 :name "de-lu-q3"
 :area "10Y1001A1001A82H"
 :quarter-offset 3
 :reference 85.00
 :spread 0.40
 :size 1.0
 :noise 0.10
 :seed 4)

;; --- Demand / surplus tilts ------------------------------------------------
;;
;; demand shifts the bid up (the MM wants to buy harder); surplus
;; shifts the ask down (the MM wants to sell harder). Uncomment to
;; bias the simulated market.
;;
;; (set-mm-demand "de-lu-q2" 0.20)   ;; peak quarter: aggressive procurement
;; (set-mm-surplus "de-lu-q3" 0.30)  ;; midday solar dump

;; --- Scheduled callbacks ---------------------------------------------------
;;
;; Demand on h0 ramps from 0 to 0.40 EUR/MWh over the first 60 seconds
;; — a slow procurement curve example. Comment out for a flat market.
;;
;; (let ((step 0))
;;   (every
;;    :milliseconds 5000
;;    :call (lambda ()
;;            (setq step (min 12 (+ step 1)))
;;            (set-mm-demand "de-lu-q0" (* step 0.033)))))
