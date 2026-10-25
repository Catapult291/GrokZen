;; Rust symbol definitions (tree-sitter-rust 0.24).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; Based on upstream `queries/tags.scm`, but `function_item` is split by
;; where it lives so free functions and methods never double-report: upstream
;; tags every `function_item` as a function *and* every one inside a
;; `declaration_list` as a method. `declaration_list` is also the body of
;; `mod` and `extern` blocks, so those are spelled out as functions here.

;; --------------------------------------------------------- free functions
(source_file
  (function_item
    name: (identifier) @name) @definition.function)

(mod_item
  body: (declaration_list
    (function_item
      name: (identifier) @name) @definition.function))

;; Nested `fn` inside a function body / any block.
(block
  (function_item
    name: (identifier) @name) @definition.function)

;; `extern "C" { fn c_fn(); }`
(foreign_mod_item
  body: (declaration_list
    (function_signature_item
      name: (identifier) @name) @definition.function))

;; ---------------------------------------------------------------- methods
(impl_item
  body: (declaration_list
    (function_item
      name: (identifier) @name) @definition.method))

;; Trait items: required (`fn f(&self);`) and provided (`fn f(&self) {}`).
(trait_item
  body: (declaration_list
    [
      (function_item
        name: (identifier) @name)
      (function_signature_item
        name: (identifier) @name)
    ] @definition.method))

;; ------------------------------------------------------------------- ADTs
(struct_item
  name: (type_identifier) @name) @definition.struct

(union_item
  name: (type_identifier) @name) @definition.struct

(enum_item
  name: (type_identifier) @name) @definition.enum

(trait_item
  name: (type_identifier) @name) @definition.trait

(type_item
  name: (type_identifier) @name) @definition.type

;; ----------------------------------------------------------------- values
(const_item
  name: (identifier) @name) @definition.const

;; Immutable `static` stays Const. `static mut` is a mutable binding, not a
;; constant; the regex is on the item's text so `&mut T` in the type does not
;; flip an immutable static into a Variable (`static FOO: &mut T` has no
;; `static mut` token pair).
(static_item
  (mutable_specifier)
  name: (identifier) @name) @definition.variable

((static_item
  name: (identifier) @name) @definition.const
  (#not-match? @definition.const "\\bstatic\\s+mut\\b"))

;; ----------------------------------------------------------------- fields
;; Named struct / union / enum-variant fields. Tuple structs have no names
;; (`ordered_field_declaration_list`) and are skipped. `exported` is filled
;; in by consumers from `visibility_modifier`; both `pub` and private fields
;; are reported.
(field_declaration
  name: (field_identifier) @name) @definition.field

(enum_variant
  name: (identifier) @name) @definition.field

;; ----------------------------------------------------------------- macros
;; `macro_rules! name { ... }`. Consumers map `definition.macro` to
;; `SymbolKind::Function` (see `symbol_kind_for_capture`).
(macro_definition
  name: (identifier) @name) @definition.macro

;; ---------------------------------------------------------------- modules
;; Both `mod x;` and `mod x { ... }`.
(mod_item
  name: (identifier) @name) @definition.module
