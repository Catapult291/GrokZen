;; Rust call sites (tree-sitter-rust 0.24).
;;
;; `@reference.call` on the call / macro node, `@name` on the callee's final
;; identifier: `foo()`, `Type::assoc()`, `value.method()`, `f::<T>()`,
;; `println!()`, `path::to::mac!()`. Name-based only.

(call_expression
  function: [
    (identifier) @name
    (scoped_identifier
      name: (identifier) @name)
    (field_expression
      field: (field_identifier) @name)
    (generic_function
      function: [
        (identifier) @name
        (scoped_identifier
          name: (identifier) @name)
        (field_expression
          field: (field_identifier) @name)
      ])
  ]) @reference.call

(macro_invocation
  macro: [
    (identifier) @name
    (scoped_identifier
      name: (identifier) @name)
  ]) @reference.call
