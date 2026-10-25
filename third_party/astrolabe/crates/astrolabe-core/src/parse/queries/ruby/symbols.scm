; Ruby symbols. Upstream tree-sitter-ruby tags.scm, extended to every
; declaration form: classes, modules (plain and `A::B` scoped names),
; instance / singleton methods (incl. operators and `x=` setters), constants
; (`X = …`, `X ||= …`, `A::X = …`, `X, Y = …`), `attr_*` accessors,
; `alias` / `alias_method`, and `define_method`.
; Top-level `def` becomes a Function in `parse/ruby.rs`; `:sym` names lose
; their colon there as well.

;; ------------------------------------------------------------ types
(class name: (constant) @name) @definition.class
(class name: (scope_resolution name: (_) @name)) @definition.class
(module name: (constant) @name) @definition.module
(module name: (scope_resolution name: (_) @name)) @definition.module

;; ---------------------------------------------------------- methods
(method name: (_) @name) @definition.method
(singleton_method name: (_) @name) @definition.method

(alias name: (_) @name) @definition.method

(call
  receiver: (_)? @_recv
  method: (identifier) @_m
  arguments: (argument_list . [(simple_symbol) (string)] @name)
  (#match? @_m "^(alias_method|define_method|define_singleton_method)$")) @definition.method

;; -------------------------------------------------------- accessors
(call
  method: (identifier) @_m
  arguments: (argument_list [(simple_symbol) (string)] @name)
  (#match? @_m "^(attr|attr_reader|attr_writer|attr_accessor|cattr_accessor|cattr_reader|cattr_writer|mattr_accessor|mattr_reader|mattr_writer)$")) @definition.field

;; -------------------------------------------------------- constants
(assignment left: (constant) @name) @definition.const
(assignment left: (scope_resolution name: (constant) @name)) @definition.const
(operator_assignment left: (constant) @name) @definition.const
(assignment left: (left_assignment_list (constant) @name)) @definition.const
