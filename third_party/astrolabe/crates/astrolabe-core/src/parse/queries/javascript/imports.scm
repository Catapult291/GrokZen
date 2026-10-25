;; JavaScript / JSX import specifiers (tree-sitter-javascript 0.25).
;;
;; `@import` — the module specifier with quotes already stripped (capture is
;; the `string_fragment`, not the literal).
;;   import a, { b } from 'x';   import 'x';
;;   export { a } from 'x';      export * from 'x';
;;   import('x')                 require('x')

(import_statement
  source: (string
    (string_fragment) @import))

(export_statement
  source: (string
    (string_fragment) @import))

;; Dynamic import.
(call_expression
  function: (import)
  arguments: (arguments
    . (string
      (string_fragment) @import)))

;; CommonJS.
((call_expression
  function: (identifier) @_fn
  arguments: (arguments
    . (string
      (string_fragment) @import)))
  (#eq? @_fn "require"))
