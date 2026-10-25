;; Lua definitions (tree-sitter-lua 0.5, Lua 5.1–5.5).
;;
;; Covered:
;;   function a() / local function a() / global function a()   -> function
;;   function M.a() / function M.b.a()                          -> function (name = last segment)
;;   function M:a()                                             -> method
;;   chunk-level `local a, b = ...` / `local a` / `X = ...` / `M.x = ...`
;;       -> variable; `lua.rs` re-kinds function values to function and
;;          dotted targets (`M.x = 1`) to field.
;;   nested `local f = function` / `M.f = function` (inside any block)  -> function
;;   `{ k = function() end }` at any depth                       -> function
;;   one level of `k = v` fields in a chunk-level table (declaration value or
;;   `return { ... }`) -> field. Function-valued fields are excluded here so
;;   the any-depth pattern reports them exactly once.

(function_declaration
  name: [
    (identifier) @name
    (dot_index_expression
      field: (identifier) @name)
  ]) @definition.function

(function_declaration
  name: (method_index_expression
    method: (identifier) @name)) @definition.method

;; ---- chunk-level bindings ------------------------------------------------

(chunk
  (variable_declaration
    (assignment_statement
      (variable_list
        name: (identifier) @name))) @definition.variable)

(chunk
  (variable_declaration
    (variable_list
      name: (identifier) @name)) @definition.variable)

(chunk
  (assignment_statement
    (variable_list
      name: [
        (identifier) @name
        (dot_index_expression
          field: (identifier) @name)
        (bracket_index_expression
          field: (string content: (string_content) @name))
      ])) @definition.variable)

;; ---- nested function values ----------------------------------------------

(block
  (variable_declaration
    (assignment_statement
      (variable_list
        .
        name: (identifier) @name)
      (expression_list
        .
        value: (function_definition)))) @definition.function)

(block
  (assignment_statement
    (variable_list
      .
      name: [
        (identifier) @name
        (dot_index_expression
          field: (identifier) @name)
        (bracket_index_expression
          field: (string content: (string_content) @name))
      ])
    (expression_list
      .
      value: (function_definition))) @definition.function)

;; ---- table fields --------------------------------------------------------

(field
  name: (identifier) @name
  value: (function_definition)) @definition.function

(chunk
  (variable_declaration
    (assignment_statement
      (expression_list
        (table_constructor
          (field
            name: (identifier) @name
            value: [
              (binary_expression) (false) (function_call) (nil) (number)
              (parenthesized_expression) (string) (table_constructor) (true)
              (unary_expression) (vararg_expression) (identifier)
              (dot_index_expression) (bracket_index_expression)
            ]) @definition.field)))))

(chunk
  (assignment_statement
    (expression_list
      (table_constructor
        (field
          name: (identifier) @name
          value: [
            (binary_expression) (false) (function_call) (nil) (number)
            (parenthesized_expression) (string) (table_constructor) (true)
            (unary_expression) (vararg_expression) (identifier)
            (dot_index_expression) (bracket_index_expression)
          ]) @definition.field))))

(chunk
  (return_statement
    (expression_list
      (table_constructor
        (field
          name: (identifier) @name
          value: [
            (binary_expression) (false) (function_call) (nil) (number)
            (parenthesized_expression) (string) (table_constructor) (true)
            (unary_expression) (vararg_expression) (identifier)
            (dot_index_expression) (bracket_index_expression)
          ]) @definition.field))))
