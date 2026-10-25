;; Java call sites (tree-sitter-java 0.23).
;;
;; `@reference.call` on the call node, `@name` on the callee's simple name.
;; `new Foo()`, `new a.b.Foo<T>()` report the constructed type's simple name so
;; constructor calls link to the class symbol.

(method_invocation
  name: (identifier) @name) @reference.call

(object_creation_expression
  type: [
    (type_identifier) @name
    (generic_type
      (type_identifier) @name)
    (scoped_type_identifier
      (type_identifier) @name .)
    (generic_type
      (scoped_type_identifier
        (type_identifier) @name .))
  ]) @reference.call
