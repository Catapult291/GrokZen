;; JavaScript / JSX call sites (tree-sitter-javascript 0.25).
;;
;; `@reference.call` on the call node, `@name` on the callee's final
;; identifier. `require(...)` and `import(...)` are imports and are excluded.

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
