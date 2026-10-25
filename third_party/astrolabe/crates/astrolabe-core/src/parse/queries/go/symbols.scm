;; Go symbol definitions (tree-sitter-go 0.25).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; Based on upstream `queries/tags.scm`; `type_spec` is split by its `type:`
;; so struct / interface / other types get distinct kinds without overlap.

;; -------------------------------------------------------------- functions
(function_declaration
  name: (identifier) @name) @definition.function

;; ---------------------------------------------------------------- methods
(method_declaration
  name: (field_identifier) @name) @definition.method

;; Interface method sets: `type Shape interface { Area() float64 }`.
(interface_type
  (method_elem
    name: (field_identifier) @name) @definition.method)

;; ------------------------------------------------------------------ types
(type_spec
  name: (type_identifier) @name
  type: (struct_type)) @definition.struct

(type_spec
  name: (type_identifier) @name
  type: (interface_type)) @definition.interface

;; Everything else declared with `type X <T>` (named types, generic instances,
;; func / map / slice / pointer types, ...).
(type_spec
  name: (type_identifier) @name
  type: [
    (array_type)
    (channel_type)
    (function_type)
    (generic_type)
    (map_type)
    (negated_type)
    (pointer_type)
    (qualified_type)
    (slice_type)
    (type_identifier)
    (parenthesized_type)
  ]) @definition.type

;; `type X = Y`
(type_alias
  name: (type_identifier) @name) @definition.type

;; --------------------------------------------------- package-level values
;; Local `var`s inside function bodies are skipped (too noisy); local
;; `const`s are kept: they are named, rarely shadowed, and `find_symbol` on
;; them must not come back empty.
(source_file
  (const_declaration
    (const_spec
      name: (identifier) @name) @definition.const))

(source_file
  (var_declaration
    (var_spec
      name: (identifier) @name) @definition.variable))

(source_file
  (var_declaration
    (var_spec_list
      (var_spec
        name: (identifier) @name) @definition.variable)))

;; ------------------------------------------------------------------ fields
;; Named struct fields (`X, Y int` yields one match per name). Embedded
;; fields have no `name:` and are handled below so `X int` is never also
;; reported as a field named `int`.
(field_declaration
  name: (field_identifier) @name) @definition.field

(field_declaration
  !name
  type: [
    (type_identifier) @name
    (qualified_type
      name: (type_identifier) @name)
    (generic_type
      type: [
        (type_identifier) @name
        (qualified_type
          name: (type_identifier) @name)
      ])
    (pointer_type
      (type_identifier) @name)
    (pointer_type
      (qualified_type
        name: (type_identifier) @name))
  ]) @definition.field

;; Interface members that are not methods: a single embedded type
;; (`io.Reader`, `Closer`). Unions (`int | string`) have more than one
;; child and are skipped.
(interface_type
  (type_elem
    .
    [
      (type_identifier) @name
      (qualified_type
        name: (type_identifier) @name)
    ]
    .) @definition.field)

;; Function-local constants (any nesting depth inside a block).
(statement_list
  (const_declaration
    (const_spec
      name: (identifier) @name) @definition.const))
