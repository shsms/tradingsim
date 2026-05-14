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
;; In reality such markets trade them as one DE-LU bidding zone; here we split
;; them so per-region liquidity profiles are observable. Volume share
;; is roughly TenneT ~40% > Amprion ~30% > 50Hertz ~20% > TransnetBW
;; ~10%, and the size tables below track that.
;;
;; Per row:
;;   (eic, prefix, mm-sizes-per-band, ag-sizes-per-profile)

(setq areas
      '(("10YDE-EON------1"   "tn"  (1.5 1.1 0.7 0.4)  (0.3 0.7 1.4 2.0))
        ("10YDE-RWENET---I"   "am"  (1.2 0.9 0.6 0.4)  (0.2 0.5 1.0 1.4))
        ("10YDE-VE-------2"   "hz"  (0.6 0.5 0.3 0.2)  (0.2 0.3 0.6 0.9))
        ("10YDE-ENBW-----N"   "bw"  (0.3 0.2 0.2 0.1)  (0.1 0.2 0.3 0.4))))

;; Markets + a single multi-area gridpool + all-pairs SIDC coupling.
(register-markets (mapcar 'car areas))
(%make-gridpool :id 1 :name "default" :areas (mapcar 'car areas))
(couple-all-pairs (mapcar 'car areas))

;; Per-area MM + aggressor fleets. Each area gets 48 MMs (one per
;; quarter, rolling forward) and 4 × 48 = 192 aggressors. Seeds are
;; auto-assigned per fleet call so RNG streams don't collide.
(dolist (entry areas)
  (mm-fleet :area (car entry)
            :prefix (cadr entry)
            :sizes (caddr entry))
  (aggressor-fleet :area (car entry)
                   :prefix (cadr entry)
                   :sizes (cadddr entry)))

;; --- Demand / surplus tilts ------------------------------------------------
;;
;; Uncomment to skew an individual MM's quoting:
;;
;; (set-mm-demand "tn-q4" 0.20)    ;; TenneT q4: aggressive procurement
;; (set-mm-surplus "am-q3" 0.30)   ;; Amprion q3: midday solar dump

;; --- Scenarios -------------------------------------------------------------
;;
;; Each script in scenarios/ is self-running on load. Uncomment any
;; line below to activate the matching market animation; each scenario
;; exposes a (scenario-NAME-stop) defun for manual cancel.
;;
;; (load "scenarios/morning-ramp.lisp")   ;; demand ramp on tn-q0
;; (load "scenarios/gate-crunch.lisp")    ;; widening spread on tn-q3
;; (load "scenarios/curtailment.lisp")    ;; supply surge on tn-q2
;; (load "scenarios/elaborate.lisp")      ;; 3-hour six-phase tour
