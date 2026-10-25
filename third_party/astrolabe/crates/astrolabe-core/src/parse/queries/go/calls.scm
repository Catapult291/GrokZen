;; Go call sites (tree-sitter-go 0.25). Same shape as upstream `tags.scm`.
;;
;; `@reference.call` on the call node, `@name` on the callee's final
;; identifier: `pkg.Fn()` and `obj.Method()` both yield the selector field.
;; Type conversions such as `int(x)` also match; that is accepted noise at the
;; `Confidence::Syntactic` level.

(call_expression
  function: [
    (identifier) @name
    (parenthesized_expression
      (identifier) @name)
    (selector_expression
      field: (field_identifier) @name)
    (parenthesized_expression
      (selector_expression
        field: (field_identifier) @name))
  ]) @reference.call
