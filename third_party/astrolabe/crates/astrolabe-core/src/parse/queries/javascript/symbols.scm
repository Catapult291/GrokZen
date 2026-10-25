;; JavaScript / JSX symbol definitions (tree-sitter-javascript 0.25).
;;
;; Mirrors `typescript/symbols.scm` minus TS-only nodes. Differences that make
;; a shared file impossible: class names are `identifier` (TS: `type_identifier`),
;; class fields are `field_definition` with a `property:` field (TS:
;; `public_field_definition` with `name:`), and `as` / `satisfies` / `!`
;; expressions do not exist.

;; -------------------------------------------------------------- functions
(function_declaration
  name: (identifier) @name) @definition.function

(generator_function_declaration
  name: (identifier) @name) @definition.function

(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression) (generator_function)]) @definition.function)

(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression) (generator_function)]) @definition.function)

;; module.exports.f = function () {} / Foo.prototype.bar = () => {} / f = () => {}
(assignment_expression
  left: [
    (identifier) @name
    (member_expression
      property: (property_identifier) @name)
  ]
  right: [(arrow_function) (function_expression) (generator_function)]) @definition.function

(pair
  key: (property_identifier) @name
  value: [(arrow_function) (function_expression) (generator_function)]) @definition.function

;; `export default function () {}` / `export default () => {}` — anonymous, so
;; the `default` keyword itself is the name, matching the language server.
(export_statement
  "default" @name
  value: [
    (function_expression !name)
    (generator_function !name)
    (arrow_function)
  ] @definition.function)

;; ---------------------------------------------------------------- classes
(class_declaration
  name: (identifier) @name) @definition.class

(class
  name: (identifier) @name) @definition.class

(variable_declarator
  name: (identifier) @name
  value: (class !name)) @definition.class

;; `export default class {}`
(export_statement
  "default" @name
  value: (class !name) @definition.class)

;; ---------------------------------------------------------------- methods
(method_definition
  name: [(property_identifier) (private_property_identifier)] @name) @definition.method

;; `class A { onClick = (e) => {} }`
(field_definition
  property: [(property_identifier) (private_property_identifier)] @name
  value: [(arrow_function) (function_expression)]) @definition.method

;; -------------------------------------------------------------- constants
;; Module-level `const` bindings whose value is not a function or class.
(program
  (lexical_declaration
    kind: "const"
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression)
      ]) @definition.const))

(export_statement
  declaration: (lexical_declaration
    kind: "const"
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression)
      ]) @definition.const))

;; -------------------------------------------------------------- variables
(program
  (lexical_declaration
    kind: "let"
    (variable_declarator
      name: (identifier) @name
      !value) @definition.variable))

(program
  (lexical_declaration
    kind: "let"
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression)
      ]) @definition.variable))

(export_statement
  declaration: (lexical_declaration
    kind: "let"
    (variable_declarator
      name: (identifier) @name
      !value) @definition.variable))

(export_statement
  declaration: (lexical_declaration
    kind: "let"
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression)
      ]) @definition.variable))

(program
  (variable_declaration
    (variable_declarator
      name: (identifier) @name
      !value) @definition.variable))

(program
  (variable_declaration
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression)
      ]) @definition.variable))

(export_statement
  declaration: (variable_declaration
    (variable_declarator
      name: (identifier) @name
      !value) @definition.variable))

(export_statement
  declaration: (variable_declaration
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression)
      ]) @definition.variable))

;; ----------------------------------------------------------------- fields
;; Class fields that are not methods. `onClick = (e) => {}` is a method
;; above; this allow-list is the complement, plus fields with no initializer.
(field_definition
  property: [(property_identifier) (private_property_identifier)] @name
  !value) @definition.field

(field_definition
  property: [(property_identifier) (private_property_identifier)] @name
  value: [
    (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
    (object) (array) (identifier) (member_expression) (subscript_expression)
    (call_expression) (new_expression) (await_expression)
    (binary_expression) (unary_expression) (ternary_expression)
    (parenthesized_expression) (this) (super)
  ]) @definition.field

;; Top-level `for (let i = 0; ...)` loop counters (typical in Vue
;; `<script setup>` and plain scripts). Only direct children of the program;
;; loops nested in functions stay local and are not indexed.
(program
  (for_statement
    initializer: (lexical_declaration
      kind: "let"
      (variable_declarator
        name: (identifier) @name) @definition.variable)))

(program
  (for_statement
    initializer: (variable_declaration
      (variable_declarator
        name: (identifier) @name) @definition.variable)))
