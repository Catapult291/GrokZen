;; Kotlin symbol definitions (tree-sitter-kotlin-ng 1.1).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; Kinds are refined in `parse/kotlin.rs::symbol_name`:
;;   * `class_declaration` -> Interface (`interface` / `fun interface`) or
;;     Enum (`enum class`); everything else stays Class.
;;   * `function_declaration` -> Method when its parent is a class body.
;;   * `property_declaration` -> Const for `const val`, Field in a class body,
;;     Variable at top level / in scripts.
;;   * `secondary_constructor` / unnamed `companion object` have no name
;;     token; the hook names them after the enclosing class / `Companion`.
;; Type patterns are unanchored, so nested, local and anonymous-object members
;; are found at any depth. Properties are anchored to a declaration scope so
;; local `val`s inside function bodies and lambdas are not indexed.

;; ------------------------------------------------------------------ types
(class_declaration
  name: (identifier) @name) @definition.class

(object_declaration
  name: (identifier) @name) @definition.class

(companion_object
  name: (identifier) @name) @definition.class

(companion_object
  !name) @definition.class

(type_alias
  type: (identifier) @name) @definition.type

;; -------------------------------------------------------------- functions
(function_declaration
  name: (identifier) @name) @definition.function

(secondary_constructor) @definition.method

;; ---------------------------------------------------------------- members
;; `class C(val a: Int, var b: String)` — constructor properties. Plain
;; parameters without `val`/`var` are not members.
(class_parameter
  ["val" "var"]
  (identifier) @name) @definition.field

(enum_entry
  (identifier) @name) @definition.const

;; ------------------------------------------------------------- properties
(class_body
  (property_declaration
    (variable_declaration
      (identifier) @name)) @definition.field)

(class_body
  (property_declaration
    (multi_variable_declaration
      (variable_declaration
        (identifier) @name))) @definition.field)

(enum_class_body
  (property_declaration
    (variable_declaration
      (identifier) @name)) @definition.field)

(source_file
  (property_declaration
    (variable_declaration
      (identifier) @name)) @definition.variable)

(source_file
  (property_declaration
    (multi_variable_declaration
      (variable_declaration
        (identifier) @name))) @definition.variable)
