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

;; --- TSO regions ----------------------------------------------------------
;;
;; Four German TSO control zones treated as separate delivery areas.
;; In reality such markets trade them as one DE-LU bidding zone; here we
;; split them so per-region liquidity profiles are observable.
;;
;; Per-area row in `areas`:
;;   (eic, prefix, mm-sizes-per-band, ag-sizes-per-profile)
;;
;;   mm-sizes-per-band     MW for quarter bands q0-11, q12-23,
;;                         q24-35, q36-47
;;   ag-sizes-per-profile  MW for aggressor profiles 0..3 (fastest
;;                         smallest → slowest largest)
;;
;; Volume share is roughly: TenneT ~40% > Amprion ~30% > 50Hertz
;; ~20% > TransnetBW ~10%. Sizes here track that share.

(setq areas
      '(("10YDE-EON------1"   "tn"  (1.5 1.1 0.7 0.4)  (0.3 0.7 1.4 2.0))
        ("10YDE-RWENET---I"   "am"  (1.2 0.9 0.6 0.4)  (0.2 0.5 1.0 1.4))
        ("10YDE-VE-------2"   "hz"  (0.6 0.5 0.3 0.2)  (0.2 0.3 0.6 0.9))
        ("10YDE-ENBW-----N"   "bw"  (0.3 0.2 0.2 0.1)  (0.1 0.2 0.3 0.4))))

;; Markets — one per area, all EUR. Default tick/step apply (0.01
;; EUR price, 0.1 MW size).
(dolist (a areas)
  (%make-market :area (car a) :currency "eur"))

;; Single gridpool spans all four areas — user-side gRPC trading
;; can pick any area; resting orders match across regions via the
;; SIDC couplings below.
(%make-gridpool
 :id 1
 :name "default"
 :areas '("10YDE-EON------1" "10YDE-RWENET---I"
          "10YDE-VE-------2" "10YDE-ENBW-----N"))

;; All-pairs coupling between the four areas (K4 = 6 edges).
(dotimes (i 4)
  (dotimes (j 4)
    (when (< i j)
      (%make-coupling
       :areas (list (car (nth i areas))
                    (car (nth j areas)))))))

;; --- MM fleet -------------------------------------------------------------
;;
;; 48 MMs per area = 192 total. Each MM rolls forward via its
;; quarter_offset so the orderbook always shows the next 12 hours of
;; 15-min contracts. Size scales by area + quarter band; spread
;; widens on far-out contracts; reference follows a gentle upward
;; intraday curve (85.00 → 89.70 EUR).

(dotimes (a 4)
  (let* ((entry (nth a areas))
         (eic (car entry))
         (label (cadr entry))
         (mm-sizes (caddr entry)))
    (dotimes (i 48)
      (let* ((band (cond ((< i 12) 0)
                         ((< i 24) 1)
                         ((< i 36) 2)
                         (t 3)))
             (sz (nth band mm-sizes))
             (sp (cond ((< i 12) 0.40)
                       ((< i 24) 0.55)
                       ((< i 36) 0.70)
                       (t 0.90))))
        (%make-market-maker
         :name (format "%s-q%d" label i)
         :area eic
         :quarter-offset i
         :reference (+ 85.0 (* 0.10 i))
         :spread sp
         :size sz
         :noise 0.10
         :seed (+ 1 (* a 1000) i))))))

;; --- Aggressors -----------------------------------------------------------
;;
;; 4 profiles per (area, quarter) — fastest+smallest through
;; slowest+largest. Profile rates scale linearly with quarter
;; offset so imminent contracts churn busily and far-out ones tick
;; quietly:
;;
;;   profile 0 — base 500  ms (q0 fires twice/sec; q47 every 24 s)
;;   profile 1 — base 1500 ms
;;   profile 2 — base 3500 ms
;;   profile 3 — base 8000 ms (jumbo, rare)
;;
;; 4 areas × 48 quarters × 4 profiles = 768 aggressors total.

(dotimes (a 4)
  (let* ((entry (nth a areas))
         (eic (car entry))
         (label (cadr entry))
         (ag-sizes (cadddr entry)))
    (dotimes (i 48)
      (dotimes (p 4)
        (%make-aggressor
         :name (format "ag-%s-q%d-%d" label i p)
         :area eic
         :quarter-offset i
         :rate-ms (* (nth p '(500 1500 3500 8000)) (+ i 1))
         :size (nth p ag-sizes)
         :side-bias 0.5
         :seed (+ 10000 (* a 100000) (* i 100) p))))))

;; --- Reference drift -------------------------------------------------------
;;
;; Tie every MM's reference to the last public trade on its contract
;; so prices migrate with activity. 0.10 = 10% pull each refresh.

(dotimes (a 4)
  (let ((label (cadr (nth a areas))))
    (dotimes (i 48)
      (set-mm-follow-last-trade (format "%s-q%d" label i) 0.10))))

;; --- Demand / surplus tilts ------------------------------------------------
;;
;; Uncomment to skew an individual MM's quoting:
;;
;; (set-mm-demand "tn-q4" 0.20)    ;; TenneT q4: aggressive procurement
;; (set-mm-surplus "am-q3" 0.30)   ;; Amprion q3: midday solar dump

;; --- Scenarios -------------------------------------------------------------
;;
;; Each script in scenarios/ is self-running on load. Uncomment any
;; line below to activate the matching market animation; each
;; scenario exposes a (scenario-NAME-stop) defun for manual cancel.
;;
;; (load "scenarios/morning-ramp.lisp")   ;; demand ramp on tn-q0
;; (load "scenarios/gate-crunch.lisp")    ;; widening spread on tn-q3
;; (load "scenarios/curtailment.lisp")    ;; supply surge on tn-q2
;; (load "scenarios/elaborate.lisp")      ;; 3-hour six-phase tour
