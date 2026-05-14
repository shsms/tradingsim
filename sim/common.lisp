;; tradingsim runtime helpers. Load from config.lisp:
;;
;;   (unless (boundp 'tradingsim-loaded)
;;     (setq tradingsim-loaded t)
;;     (load "sim/common.lisp"))
;;
;; Built on tulisp-async's `run-with-timer` / `cancel-timer`. Use
;; `(every :milliseconds N :call FN)` to schedule a periodic callback
;; that nudges market-maker knobs over time.

;; Active timers — tracked so (reset-state) can cancel them on hot
;; reload.
(unless (boundp 'active-timers)
  (setq active-timers nil))

(defun reset-state ()
  "Cancel every timer registered via (every …) and reset the bookkeeping.
Call at the top of config.lisp so a hot reload starts from a clean
slate; the running market-maker tasks keep going (their SharedConfig
handles survive across reloads)."
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
;; Fleet helpers — declarative wrappers around the (%make-market …),
;; (%make-coupling …), (%make-market-maker …) and (%make-aggressor …)
;; primitives. Pull the dotimes / band-mapping boilerplate out of
;; config.lisp so each area only needs to declare what's distinct
;; about it.
;; ---------------------------------------------------------------------------

(defun register-markets (eics &rest plist)
  "Register one market per EIC in `eics`. :currency defaults to \"eur\"."
  (let ((currency (or (plist-get plist :currency) "eur")))
    (dolist (e eics)
      (%make-market :area e :currency currency))))

(defun couple-all-pairs (eics)
  "Couple every distinct pair of EICs in `eics` (K_n graph)."
  (let ((n (length eics)))
    (dotimes (i n)
      (dotimes (j n)
        (when (< i j)
          (%make-coupling
           :areas (list (nth i eics) (nth j eics))))))))

(defun mm-fleet (&rest plist)
  "Spawn :quarters market-makers covering one delivery area — one per
15-min contract. Each MM's quarter-offset rolls forward in the
binary's spawn task so the fleet always covers the next-N quarters.

  :area             EIC code (required)
  :prefix           name prefix (required, e.g. \"tn\" → \"tn-q0\")
  :quarters         contracts to cover (default 48)
  :sizes            list of MM sizes per band, MW
                    (default '(1.0 0.7 0.5 0.3); 4 bands × 12 quarters)
  :spreads          list of spreads per band, EUR
                    (default '(0.40 0.55 0.70 0.90))
  :reference-base   reference at q0, EUR (default 85.0)
  :reference-slope  reference walk per quarter, EUR (default 0.10)
  :noise            random-walk noise on the reference (default 0.10)
  :follow           follow-last-trade rate (default 0.10; 0 = static)
  :seed-base        starting seed (default: auto from a global counter)

The band index for quarter i is `(* i n) / quarters` where n is the
number of entries in :sizes — so a 4-element list maps to 4 evenly
spaced bands across the window."
  (let* ((area (plist-get plist :area))
         (prefix (plist-get plist :prefix))
         (quarters (or (plist-get plist :quarters) 48))
         (sizes (or (plist-get plist :sizes) '(1.0 0.7 0.5 0.3)))
         (spreads (or (plist-get plist :spreads) '(0.40 0.55 0.70 0.90)))
         (ref-base (or (plist-get plist :reference-base) 85.0))
         (ref-slope (or (plist-get plist :reference-slope) 0.10))
         (noise (or (plist-get plist :noise) 0.10))
         (follow (or (plist-get plist :follow) 0.10))
         (seed-base (or (plist-get plist :seed-base) (fleet-next-seed-base)))
         (band-count (length sizes)))
    (dotimes (i quarters)
      (let* ((band (min (- band-count 1) (/ (* i band-count) quarters)))
             (name (format "%s-q%d" prefix i)))
        (%make-market-maker
         :name name
         :area area
         :quarter-offset i
         :reference (+ ref-base (* ref-slope i))
         :spread (nth band spreads)
         :size (nth band sizes)
         :noise noise
         :seed (+ seed-base i))
        (when (> follow 0)
          (set-mm-follow-last-trade name follow))))))

(defun aggressor-fleet (&rest plist)
  "Spawn :quarters × P aggressors covering one delivery area, where
P is the length of :sizes (one profile per entry). Names follow
`ag-<prefix>-q<quarter>-<profile>`.

  :area         EIC code (required)
  :prefix       name prefix (required, e.g. \"tn\")
  :quarters     contracts to cover (default 48)
  :sizes        list of MW per profile (default '(0.2 0.5 1.0 1.5))
  :rates-base   list of base rate_ms per profile
                (default '(500 1500 3500 8000));
                effective rate = base × (quarter + 1)
  :side-bias    side bias for every profile (default 0.5)
  :seed-base    starting seed (default: auto from a global counter)"
  (let* ((area (plist-get plist :area))
         (prefix (plist-get plist :prefix))
         (quarters (or (plist-get plist :quarters) 48))
         (sizes (or (plist-get plist :sizes) '(0.2 0.5 1.0 1.5)))
         (rates-base (or (plist-get plist :rates-base) '(500 1500 3500 8000)))
         (side-bias (or (plist-get plist :side-bias) 0.5))
         (seed-base (or (plist-get plist :seed-base) (fleet-next-seed-base)))
         (profiles (length sizes)))
    (dotimes (i quarters)
      (dotimes (p profiles)
        (%make-aggressor
         :name (format "ag-%s-q%d-%d" prefix i p)
         :area area
         :quarter-offset i
         :rate-ms (* (nth p rates-base) (+ i 1))
         :size (nth p sizes)
         :side-bias side-bias
         :seed (+ seed-base (* i 100) p))))))
