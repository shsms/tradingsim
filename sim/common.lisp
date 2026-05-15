;; tradingsim runtime helpers. Load from config.lisp:
;;
;;   (unless (boundp 'tradingsim-loaded)
;;     (setq tradingsim-loaded t)
;;     (load "sim/common.lisp"))
;;
;; Built on tulisp-async's `run-with-timer` / `cancel-timer`. Use
;; `(every :milliseconds N :call FN)` to schedule a periodic callback
;; — useful for drifting weather knobs, advancing a scenario, or any
;; other time-driven mutation that needs to keep firing across hot
;; reloads (active timers are tracked so (reset-state) can cancel
;; them before the new config registers fresh ones).

;; Active timers — tracked so (reset-state) can cancel them on hot
;; reload.
(unless (boundp 'active-timers)
  (setq active-timers nil))

(defun reset-state ()
  "Cancel every timer registered via (every …) and reset the bookkeeping.
Call at the top of config.lisp so a hot reload starts from a clean
slate; the running per-contract counterparty tasks keep going (each
fleet's SharedFleetParams and each MM's SharedConfig survive the
reload, and re-firing the fleet primitive mutates them in place)."
  (dolist (tm active-timers)
    (cancel-timer tm))
  (setq active-timers nil)
  (setq *fleet-seed-counter* 0))

;; Monotonic counter so fleet helpers can auto-assign disjoint seed
;; ranges per call. Reset on (reset-state) so seeds are deterministic
;; across hot reloads.
(unless (boundp '*fleet-seed-counter*)
  (setq *fleet-seed-counter* 0))

(defun fleet-next-seed-base ()
  "Reserve the next 1000-id seed window and return its start."
  (setq *fleet-seed-counter* (+ *fleet-seed-counter* 1))
  (* *fleet-seed-counter* 1000))

(defun every (&rest plist)
  "Call :call every :milliseconds ms. First firing happens after the
interval elapses — not synchronously at load time — so an (every)
block can sit anywhere in the config relative to what it references."
  (let* ((ms (plist-get plist :milliseconds))
         (func (plist-get plist :call))
         (args (plist-get plist :args))
         (secs (/ ms 1000.0)))
    (setq active-timers
          (cons (apply 'run-with-timer secs secs func args)
                active-timers))))

;; ---------------------------------------------------------------------------
;; Fleet helpers — declarative wrappers around (%make-market …),
;; (%make-coupling …), (%make-mm-fleet …) and (%make-aggressor-fleet …).
;; Each wrapper pulls the keyword-handling / default-application
;; boilerplate out of config.lisp so an area declaration only needs
;; to mention what's distinct about it.
;; ---------------------------------------------------------------------------

(defun register-markets (eics &rest plist)
  "Register one market per EIC in `eics`. :currency defaults to \"eur\"."
  (let ((currency (or (plist-get plist :currency) "eur")))
    (dolist (e eics)
      (%make-market :area e :currency currency))))

(defun couple-all-pairs (eics &rest plist)
  "Couple every distinct pair of EICs in `eics` (K_n graph). Pass
:gate-offset-seconds N to set a cross-border gate (defaults to 0,
meaning intra-zone — closes at the regular delivery start)."
  (let ((offset (or (plist-get plist :gate-offset-seconds) 0))
        (n (length eics)))
    (dotimes (i n)
      (dotimes (j n)
        (when (< i j)
          (%make-coupling
           :areas (list (nth i eics) (nth j eics))
           :gate-offset-seconds offset))))))

(defun couple-pairs-across (a-list b-list &rest plist)
  "Couple every EIC in `a-list` to every EIC in `b-list` (Cartesian
product). Same :gate-offset-seconds knob as couple-all-pairs."
  (let ((offset (or (plist-get plist :gate-offset-seconds) 0)))
    (dolist (a a-list)
      (dolist (b b-list)
        (%make-coupling
         :areas (list a b)
         :gate-offset-seconds offset)))))

(defun mm-fleet (&rest plist)
  "Register one MM fleet covering :quarters forward 15-min contracts
in :area. FleetManager spawns one MM per contract in the rolling
window and rotates them every 15 min so contracts gating off are
retired and fresh ones at the far edge come online.

  :area         EIC code (required)
  :prefix       name prefix (required, e.g. \"tn\")
  :quarters     contracts to cover (default 48)
  :sizes        list of MM sizes per band, MW
                (default '(1.0 0.7 0.5 0.3); 4 bands across the window)
  :spreads      list of half-spreads per band, EUR
                (default '(0.40 0.55 0.70 0.90))
  :noise        random-walk noise on the reference (default 0.10)
  :follow       follow-last-trade rate (default 0.10; 0 = static)
  :refresh-ms   per-MM refresh cadence in ms (default 2000)
  :seed-base    starting seed (default: auto from a global counter)

Band index for a contract at current offset Q is
`(min (- (length sizes) 1) (/ (* Q (length sizes)) quarters))`.
A contract enters at the back band, tightens its spread and grows
its size as it ages forward."
  (let* ((area (plist-get plist :area))
         (prefix (plist-get plist :prefix))
         (quarters (or (plist-get plist :quarters) 48))
         (sizes (or (plist-get plist :sizes) '(1.0 0.7 0.5 0.3)))
         (spreads (or (plist-get plist :spreads) '(0.40 0.55 0.70 0.90)))
         (noise (or (plist-get plist :noise) 0.10))
         (follow (or (plist-get plist :follow) 0.10))
         (refresh-ms (or (plist-get plist :refresh-ms) 2000))
         (seed-base (or (plist-get plist :seed-base) (fleet-next-seed-base))))
    (%make-mm-fleet
     :name (format "%s-fleet" prefix)
     :area area
     :window-quarters quarters
     :size-bands sizes
     :spread-bands spreads
     :noise noise
     :follow follow
     :refresh-ms refresh-ms
     :seed-base seed-base)))

(defun aggressor-fleet (&rest plist)
  "Register one aggressor fleet covering :quarters forward contracts
in :area with P profiles (= (length :sizes)). FleetManager spawns
one aggressor per (contract, profile) pair, rotating per quarter
the same way mm-fleet does.

  :area         EIC code (required)
  :prefix       name prefix (required, e.g. \"tn\")
  :quarters     contracts to cover (default 48)
  :sizes        list of MW per profile (default '(0.2 0.5 1.0 1.5))
  :rates-base   list of base rate_ms per profile
                (default '(500 1500 3500 8000));
                effective rate = base × (current_offset + 1)
  :side-bias    side bias for every profile (default 0.5)
  :seed-base    starting seed (default: auto from a global counter)"
  (let* ((area (plist-get plist :area))
         (prefix (plist-get plist :prefix))
         (quarters (or (plist-get plist :quarters) 48))
         (sizes (or (plist-get plist :sizes) '(0.2 0.5 1.0 1.5)))
         (rates-base (or (plist-get plist :rates-base) '(500 1500 3500 8000)))
         (side-bias (or (plist-get plist :side-bias) 0.5))
         (seed-base (or (plist-get plist :seed-base) (fleet-next-seed-base))))
    (%make-aggressor-fleet
     :name (format "ag-%s-fleet" prefix)
     :area area
     :window-quarters quarters
     :profile-sizes sizes
     :profile-rate-bases rates-base
     :side-bias side-bias
     :seed-base seed-base)))
