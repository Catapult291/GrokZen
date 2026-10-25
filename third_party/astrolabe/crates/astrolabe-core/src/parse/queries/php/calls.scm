;; PHP call sites (tree-sitter-php 0.24).
;;
;; `@reference.call` on the call node, `@name` on the callee's final name.
;; `new Foo()` reports the constructed type so constructors link to the class.

(function_call_expression
  function: [
    (name) @name
    (qualified_name
      (name) @name)
    (variable_name
      (name) @name)
  ]) @reference.call

(member_call_expression
  name: (name) @name) @reference.call

(scoped_call_expression
  name: (name) @name) @reference.call

(object_creation_expression
  [
    (name) @name
    (qualified_name
      (name) @name)
  ]) @reference.call
