;; TypeScript / TSX import specifiers (tree-sitter-typescript 0.23).
;;
;; `@import` — the module specifier with quotes already stripped: the capture
;; is the `string_fragment` inside the string literal, so `'./x'` and `"./x"`
;; both yield `./x`.
;;   import a, { b } from 'x';   import type { T } from 'x';   import 'x';
;;   import fs = require('fs');
;;   export { a } from 'x';      export * from 'x';
;;   import('x')                 require('x')

(import_statement
  source: (string
    (string_fragment) @import))

;; `import x = require('y')` keeps its source on the clause, not the statement.
(import_statement
  (import_require_clause
    source: (string
      (string_fragment) @import)))

(export_statement
  source: (string
    (string_fragment) @import))

;; Dynamic import: `import('./lazy')`, `await import('x')`.
(call_expression
  function: (import)
  arguments: (arguments
    . (string
      (string_fragment) @import)))

;; CommonJS: `require('x')`.
((call_expression
  function: (identifier) @_fn
  arguments: (arguments
    . (string
      (string_fragment) @import)))
  (#eq? @_fn "require"))
