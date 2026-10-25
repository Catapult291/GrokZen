;; C++ call sites (tree-sitter-cpp 0.23).
;;
;; `@reference.call` on the call node, `@name` on the callee's final identifier.
;; Qualified calls `ns::fn()` and member calls `obj.m()` / `obj->m()` report the
;; final name so name-matched edges stay useful at Syntactic confidence.

(call_expression
  function: (identifier) @name) @reference.call

(call_expression
  function: (field_expression
    field: (field_identifier) @name)) @reference.call

(call_expression
  function: (qualified_identifier
    name: (identifier) @name)) @reference.call

(call_expression
  function: (template_function
    name: (identifier) @name)) @reference.call

(call_expression
  function: (parenthesized_expression
    (identifier) @name)) @reference.call
