;; C call sites (tree-sitter-c 0.24).
;;
;; `@reference.call` on the call node, `@name` on the callee's final identifier.

(call_expression
  function: (identifier) @name) @reference.call

(call_expression
  function: (field_expression
    field: (field_identifier) @name)) @reference.call

(call_expression
  function: (parenthesized_expression
    (identifier) @name)) @reference.call
