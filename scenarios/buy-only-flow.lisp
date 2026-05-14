;; scenarios/buy-only-flow.lisp — every aggressor on every quarter
;; runs side-bias 1.0. All trades happen at asks; the sell-side
;; flow vanishes from the public tape. The orderbook still shows
;; bids (MM keeps quoting; aggressors don't hit them) but the
;; depth there sits untouched while asks get vacuumed and the MM
;; reference drifts up steadily. Useful for testing a bot's
;; "where did all the sellers go?" detection logic.

(define-scenario
 :name "buy-only-flow"
 :description "Aggressor bias pinned at 1.0 all day — no sell-side trade flow at all."
 :stages
 '((:name "all buys" :hour-from 0.0 :hour-to 24.0 :bias-from 1.00 :bias-to 1.00)))
