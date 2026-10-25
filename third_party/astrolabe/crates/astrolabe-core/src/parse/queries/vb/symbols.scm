;; Visual Basic .NET symbols (tree-sitter-vb-dotnet 0.1).
;;
;; Class / Module / Structure / Interface / Enum, plus Sub and Function
;; (both `method_declaration`) and Property. Namespaces are modules.
;; A VB Module is also a module: it is the declaration, not a namespace.

(class_block
  name: (identifier) @name) @definition.class

(module_block
  name: (identifier) @name) @definition.module

(structure_block
  name: (identifier) @name) @definition.struct

(interface_block
  name: (identifier) @name) @definition.interface

(enum_block
  name: (identifier) @name) @definition.enum

(method_declaration
  name: (identifier) @name) @definition.method

(property_declaration
  name: (identifier) @name) @definition.field

(namespace_block
  name: (namespace_name) @name) @definition.module
