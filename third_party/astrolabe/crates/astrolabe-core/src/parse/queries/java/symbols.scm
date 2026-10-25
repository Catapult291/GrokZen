;; Java symbol definitions (tree-sitter-java 0.23).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; None of the type patterns are anchored to a parent, so nested / inner /
;; local / anonymous-class members are found at any depth.

;; ------------------------------------------------------------------ types
(class_declaration
  name: (identifier) @name) @definition.class

(record_declaration
  name: (identifier) @name) @definition.class

(interface_declaration
  name: (identifier) @name) @definition.interface

(annotation_type_declaration
  name: (identifier) @name) @definition.interface

(enum_declaration
  name: (identifier) @name) @definition.enum

;; ---------------------------------------------------------------- methods
(method_declaration
  name: (identifier) @name) @definition.method

(constructor_declaration
  name: (identifier) @name) @definition.method

;; `record Pair(int a, int b) { Pair { ... } }`
(compact_constructor_declaration
  name: (identifier) @name) @definition.method

;; Members of an `@interface`: `String value();`, `Policy policy() default X;`.
;; Language servers list these in document symbols, and gson declares several
;; (`postDeserialize`, `serialize`, `value`), so omitting them showed up as a
;; real recall gap rather than baseline noise.
(annotation_type_element_declaration
  name: (identifier) @name) @definition.method

;; -------------------------------------------------------------- constants
;; `static final` fields. One match per declarator, so `static final int A, B`
;; reports both names against the same declaration node.
((field_declaration
  (modifiers) @_mods
  declarator: (variable_declarator
    name: (identifier) @name)) @definition.const
  (#match? @_mods "\\bstatic\\b")
  (#match? @_mods "\\bfinal\\b"))

;; Interface fields are implicitly `public static final`.
(constant_declaration
  declarator: (variable_declarator
    name: (identifier) @name)) @definition.const

(enum_constant
  name: (identifier) @name) @definition.const

;; ------------------------------------------------------------------ fields
;; Ordinary instance / static fields. Disjoint from the `static final`
;; constant pattern above: a node that matches both `static` and `final` is
;; Const, everything else with a `modifiers` child is Field.
((field_declaration
  (modifiers) @_mods
  declarator: (variable_declarator
    name: (identifier) @name)) @definition.field
  (#not-match? @_mods "\\bstatic\\b[\\s\\S]*\\bfinal\\b")
  (#not-match? @_mods "\\bfinal\\b[\\s\\S]*\\bstatic\\b"))

;; Package-private fields with no modifiers (`int count;`). The `.` anchor
;; requires the type to be the first child, so a `modifiers` child (including
;; `static final`) cannot match here.
(field_declaration
  .
  type: (_)
  declarator: (variable_declarator
    name: (identifier) @name)) @definition.field

;; Record components are fields: `record Pair(int a, int b)`.
(record_declaration
  parameters: (formal_parameters
    (formal_parameter
      name: (identifier) @name) @definition.field))
