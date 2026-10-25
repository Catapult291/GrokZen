;; C# call sites (tree-sitter-c-sharp 0.23).
;;
;; `@reference.call` on the call node, `@name` on the callee's simple name.
;; `obj.M()`, `M()`, `M<T>()`, and `N.M()` all report `M`. `new T()` reports
;; the constructed type's simple name so constructor calls link to the type.

(invocation_expression
  function: (identifier) @name) @reference.call

(invocation_expression
  function: (generic_name
    (identifier) @name)) @reference.call

(invocation_expression
  function: (member_access_expression
    name: (identifier) @name)) @reference.call

(invocation_expression
  function: (member_access_expression
    name: (generic_name
      (identifier) @name))) @reference.call

(invocation_expression
  function: (qualified_name
    name: (identifier) @name)) @reference.call

(invocation_expression
  function: (qualified_name
    name: (generic_name
      (identifier) @name))) @reference.call

(object_creation_expression
  type: (identifier) @name) @reference.call

(object_creation_expression
  type: (generic_name
    (identifier) @name)) @reference.call

(object_creation_expression
  type: (qualified_name
    name: (identifier) @name)) @reference.call

(object_creation_expression
  type: (qualified_name
    name: (generic_name
      (identifier) @name))) @reference.call
