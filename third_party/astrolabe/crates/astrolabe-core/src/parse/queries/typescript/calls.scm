;; TypeScript / TSX call sites (tree-sitter-typescript 0.23).
;;
;; `@reference.call` on the call node, `@name` on the callee's final
;; identifier: `f()`, `obj.m()`, `a?.b.c()`, `f<T>()`, `new Foo()`.
;; `require(...)` and `import(...)` are imports, not calls, and are excluded.

((call_expression
  function: (identifier) @name) @reference.call
  (#not-eq? @name "require"))

(call_expression
  function: (member_expression
    property: [(property_identifier) (private_property_identifier)] @name)) @reference.call

(new_expression
  constructor: [
    (identifier) @name
    (member_expression
      property: (property_identifier) @name)
  ]) @reference.call
