;; tradingsim runtime helpers. Load from config.lisp:
;;
;;   (unless (boundp 'tradingsim-loaded)
;;     (setq tradingsim-loaded t)
;;     (load "sim/common.lisp"))
;;
;; Built on tulisp-async's `run-with-timer` / `cancel-timer`. Use
;; `(every :milliseconds N :call FN)` to schedule a periodic callback
;; that nudges market-maker knobs over time.

;; Active timers — tracked so a future (reset-state) can cancel them.
(unless (boundp 'active-timers)
  (setq active-timers nil))

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
