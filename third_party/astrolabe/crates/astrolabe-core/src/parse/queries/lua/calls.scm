;; Lua call sites (tree-sitter-lua 0.5).
;;
;; `@reference.call` on the call node, `@name` on the callee's last segment:
;; `f()`, `a.b.f()`, `obj:f()`, `f "str"`, `f { ... }`.

(function_call
  name: [
    (identifier) @name
    (dot_index_expression
      field: (identifier) @name)
    (method_index_expression
      method: (identifier) @name)
  ]) @reference.call
