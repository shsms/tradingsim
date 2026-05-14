;; scenarios/day-ahead-print.lisp — anchor the forward curve to a
;; specific previously-cleared day-ahead price profile. Trading
;; apps see the intraday market opening with prices on the curve
;; they expected from yesterday's clearing. Useful for testing
;; "compare intraday to day-ahead" logic.
;;
;; The (hour, price) pairs below approximate a typical German
;; weekday day-ahead clearing: deep midday belly, sharp evening
;; peak, gentle overnight decline.
;;
;; Load-time script — activate by adding (load …) to config.lisp.

(dolist (entry '(( 0  72.50)
                 ( 1  68.20)
                 ( 2  64.10)
                 ( 3  62.50)
                 ( 4  64.80)
                 ( 5  72.10)
                 ( 6  88.40)
                 ( 7 105.20)
                 ( 8 118.50)
                 ( 9  95.30)
                 (10  70.80)
                 (11  50.20)
                 (12  38.50)
                 (13  35.10)
                 (14  38.40)
                 (15  55.60)
                 (16  78.10)
                 (17 102.40)
                 (18 138.50)
                 (19 135.20)
                 (20 110.30)
                 (21  92.50)
                 (22  82.70)
                 (23  76.30)))
  (set-forward-curve-base (car entry) (cadr entry)))

(log.info "day-ahead-print: forward curve anchored to typical-weekday print")
