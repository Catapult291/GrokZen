;; PHP symbol definitions (tree-sitter-php 0.24, LANGUAGE_PHP).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; Ported from OpenVisio / upstream tags.scm into Astrolabe capture vocabulary.
;; Trait maps to `@definition.trait` (Astrolabe has Trait); upstream tags used
;; interface for traits.

(namespace_definition
  name: (namespace_name) @name) @definition.module

(class_declaration
  name: (name) @name) @definition.class

(interface_declaration
  name: (name) @name) @definition.interface

(trait_declaration
  name: (name) @name) @definition.trait

(enum_declaration
  name: (name) @name) @definition.enum

(function_definition
  name: (name) @name) @definition.function

(method_declaration
  name: (name) @name) @definition.method

(const_declaration
  (const_element
    (name) @name)) @definition.const

(property_declaration
  (property_element
    (variable_name
      (name) @name))) @definition.field
