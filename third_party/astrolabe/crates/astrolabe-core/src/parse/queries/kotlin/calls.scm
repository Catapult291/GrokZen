;; Kotlin call sites (tree-sitter-kotlin-ng 1.1).
;;
;; `@reference.call` on the call node, `@name` on the callee's simple name:
;;   foo()  foo<T>()  foo { }    -> foo
;;   a.b.bar()  a?.bar()         -> bar
;;   class C : Base()            -> Base (supertype constructor call)
;; Constructor calls look like plain calls (`Foo()`), so they link to the
;; class symbol by name.

(call_expression
  .
  (identifier) @name) @reference.call

(call_expression
  .
  (navigation_expression
    (identifier) @name .)) @reference.call

(constructor_invocation
  (user_type
    (identifier) @name .)) @reference.call
