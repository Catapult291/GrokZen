;; Swift symbol definitions (tree-sitter-swift 0.7).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; Type patterns are not anchored to a parent, so nested types are found at any
;; depth. `parse/swift.rs` refines kinds by context (a `func` inside a type body
;; becomes a Method; a top-level `let` a Const) and decides visibility.
;;
;; Extensions (`class_declaration` with `declaration_kind: "extension"`) are not
;; definitions themselves; their members are picked up by the member patterns.

;; ------------------------------------------------------------------ types
(class_declaration
  declaration_kind: "class"
  name: (type_identifier) @name) @definition.class

(class_declaration
  declaration_kind: "actor"
  name: (type_identifier) @name) @definition.class

(class_declaration
  declaration_kind: "struct"
  name: (type_identifier) @name) @definition.struct

(class_declaration
  declaration_kind: "enum"
  name: (type_identifier) @name) @definition.enum

(protocol_declaration
  name: (type_identifier) @name) @definition.interface

(typealias_declaration
  name: (type_identifier) @name) @definition.type

(associatedtype_declaration
  name: (type_identifier) @name) @definition.type

;; ------------------------------------------------------------- functions
;; The grammar also labels the return type `name:` (a `user_type`, never one of
;; the alternatives below), so the alternation keeps only the declared name:
;; an identifier, a custom operator (`<~>`), `!`, or a built-in operator token.
(function_declaration
  name: [
    (simple_identifier)
    (custom_operator)
    (bang)
    "==" "!=" "!==" "===" "<" ">" "<=" ">=" "+" "-" "*" "/" "%" "+=" "-=" "*="
    "/=" "%=" "=" "&" "|" "^" "~" "<<" ">>" "++" "--"
  ] @name) @definition.function

(protocol_function_declaration
  name: [
    (simple_identifier)
    (custom_operator)
    (bang)
    "==" "!=" "!==" "===" "<" ">" "<=" ">=" "+" "-" "*" "/" "%" "+=" "-=" "*="
    "/=" "%=" "=" "&" "|" "^" "~" "<<" ">>" "++" "--"
  ] @name) @definition.method

(init_declaration
  "init" @name) @definition.method

(deinit_declaration
  "deinit" @name) @definition.method

(subscript_declaration
  "subscript" @name) @definition.method

(macro_declaration
  (simple_identifier) @name) @definition.macro

;; ------------------------------------------------------------- enum cases
;; `case a, b(Int)` — one match per case name.
(enum_entry
  name: (simple_identifier) @name) @definition.const

;; ------------------------------------------------- properties / variables
;; Only type members and file-scope bindings; locals inside function bodies,
;; closures and accessors are not symbols. One match per bound name, so
;; `let a = 1, b = 2` and `var (x, y) = ...` report every name.
(class_body
  (property_declaration
    name: (pattern
      bound_identifier: (simple_identifier) @name)) @definition.field)

(class_body
  (property_declaration
    name: (pattern
      (pattern
        (simple_identifier) @name))) @definition.field)

(enum_class_body
  (property_declaration
    name: (pattern
      bound_identifier: (simple_identifier) @name)) @definition.field)

(enum_class_body
  (property_declaration
    name: (pattern
      (pattern
        (simple_identifier) @name))) @definition.field)

(source_file
  (property_declaration
    name: (pattern
      bound_identifier: (simple_identifier) @name)) @definition.variable)

(source_file
  (property_declaration
    name: (pattern
      (pattern
        (simple_identifier) @name))) @definition.variable)

(protocol_property_declaration
  name: (pattern
    bound_identifier: (simple_identifier) @name)) @definition.field
