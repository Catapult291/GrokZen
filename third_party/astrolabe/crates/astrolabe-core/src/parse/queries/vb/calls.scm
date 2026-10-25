;; Visual Basic .NET calls (tree-sitter-vb-dotnet 0.1).
;;
;; `@reference.call` on the call node, `@name` on the callee's simple name.
;; `New T()` reports the constructed type's last identifier.

(invocation
  target: (identifier) @name) @reference.call

(invocation
  target: (member_access
    member: (identifier) @name)) @reference.call

(new_expression
  type: (type
    (namespace_name
      (identifier) @name .))) @reference.call

(new_expression
  type: (type
    (generic_type
      (namespace_name
        (identifier) @name .)))) @reference.call

(new_expression
  type: (type
    (array_type
      (namespace_name
        (identifier) @name .)))) @reference.call
