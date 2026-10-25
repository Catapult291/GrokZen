;; Swift call sites (tree-sitter-swift 0.7).
;;
;; `@reference.call` on the call node, `@name` on the callee's final identifier:
;;   f(x)          -> f
;;   Foo()         -> Foo      (initialiser call links to the type)
;;   a.b.c()       -> c
;;   x?.y { ... }  -> y        (trailing closure)

(call_expression
  .
  (simple_identifier) @name) @reference.call

(call_expression
  .
  (navigation_expression
    suffix: (navigation_suffix
      suffix: (simple_identifier) @name))) @reference.call
