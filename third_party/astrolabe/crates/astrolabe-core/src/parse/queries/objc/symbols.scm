;; Objective-C / Objective-C++ symbol definitions (tree-sitter-objc 3.0).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; Names are finalised in `parse/objc.rs::symbol_name`:
;;   - methods  -> full selector (`initWithURL:options:`), `+`/`-` dropped
;;   - categories -> `Class(Category)`; class extensions keep `Class`
;;   - properties / ivars -> innermost declarator identifier
;;   - file-scope `const` variables -> Const
;; Forward declarations (`@class A;`, `@protocol P;`) are not definitions.

;; ------------------------------------------------------------ ObjC types
(class_interface . (identifier) @name) @definition.class
(class_implementation . (identifier) @name) @definition.class
(protocol_declaration . (identifier) @name) @definition.interface
(compatibility_alias_declaration alias: (identifier) @name) @definition.type

;; --------------------------------------------------------------- methods
;; `.` anchors skip the anonymous `-`/`+`; the first selector keyword is
;; `@name`, the hook rebuilds the full selector from the definition node.
(method_declaration . (method_type) . (identifier) @name) @definition.method
(method_declaration . (identifier) @name) @definition.method
(method_definition . (method_type) . (identifier) @name) @definition.method
(method_definition . (identifier) @name) @definition.method

;; ------------------------------------------------- properties and ivars
(property_declaration
  (struct_declaration (struct_declarator) @name)) @definition.field
(instance_variable
  (struct_declaration (struct_declarator) @name)) @definition.field

;; ------------------------------------------------------------- functions
(function_definition
  declarator: (function_declarator declarator: (identifier) @name)) @definition.function
(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator declarator: (identifier) @name))) @definition.function
(function_definition
  declarator: (pointer_declarator
    declarator: (pointer_declarator
      declarator: (function_declarator declarator: (identifier) @name)))) @definition.function
(function_definition
  declarator: (block_pointer_declarator
    declarator: (function_declarator declarator: (identifier) @name))) @definition.function

;; ------------------------------------------------------------- C types
(struct_specifier name: (type_identifier) @name body: (_)) @definition.struct
(union_specifier name: (type_identifier) @name body: (_)) @definition.type
(enum_specifier name: (type_identifier) @name body: (_)) @definition.enum

;; `@name` is the whole declarator; the hook peels it to the type name
;; (`typedef void (^AFBlock)(id)`, `typedef int * _Nullable (*Fn)(void)`).
(type_definition declarator: (_) @name) @definition.type

;; ---------------------------------------------------- members / constants
(enumerator name: (identifier) @name) @definition.const

;; Struct / union fields; the hook peels the declarator to its field name.
(field_declaration declarator: (_) @name) @definition.field

;; Macros: object-like `#define` is a constant, function-like a macro.
(preproc_def name: (identifier) @name) @definition.const
(preproc_function_def name: (identifier) @name) @definition.macro

;; ------------------------------------- file-scope declarations
;; Variables, constants and function prototypes at file scope (directly, or
;; under `#if` / `extern "C"`), never locals. `@name` is the declarator; the
;; hook (`objc.rs::symbol_name`) peels it to the identifier and turns
;; prototypes into Function and `const` variables into Const. One wildcard
;; pattern per parent keeps query compilation cheap on this large grammar.
(translation_unit (declaration declarator: (_) @name) @definition.variable)
(preproc_if (declaration declarator: (_) @name) @definition.variable)
(preproc_ifdef (declaration declarator: (_) @name) @definition.variable)
(preproc_else (declaration declarator: (_) @name) @definition.variable)
(preproc_elif (declaration declarator: (_) @name) @definition.variable)
(linkage_specification
  body: (declaration_list (declaration declarator: (_) @name) @definition.variable))
(linkage_specification body: (declaration declarator: (_) @name) @definition.variable)
