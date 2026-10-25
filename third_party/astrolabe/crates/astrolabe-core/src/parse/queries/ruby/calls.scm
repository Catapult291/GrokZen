; Ruby call sites: `foo(…)`, `a.b`, `a&.b`, `A::b`, `super`-less. Command
; calls without parens (`puts x`) are `call` nodes too. Bare identifiers
; with no args/receiver are indistinguishable from locals and are skipped.
(call method: [(identifier) (constant) (operator)] @name) @reference.call
