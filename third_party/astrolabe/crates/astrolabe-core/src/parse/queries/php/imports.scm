;; PHP import / include specifiers (tree-sitter-php 0.24).
;;
;; `@import` — `use` clause path (namespace\Name) or include/require path.
;; Double-quoted paths are `encapsed_string` with `string_content`; single-quoted
;; are `string` with `string_content`. Capturing `string_content` avoids quotes;
;; whole-string fallbacks still work via `extract_imports` `unquote`.

(namespace_use_clause
  [
    (name) @import
    (qualified_name) @import
  ])

(include_expression
  [
    (string (string_content) @import)
    (encapsed_string (string_content) @import)
  ])

(include_once_expression
  [
    (string (string_content) @import)
    (encapsed_string (string_content) @import)
  ])

(require_expression
  [
    (string (string_content) @import)
    (encapsed_string (string_content) @import)
  ])

(require_once_expression
  [
    (string (string_content) @import)
    (encapsed_string (string_content) @import)
  ])

; `__DIR__ . '/x.php'` and `dirname(__DIR__, n) . '/x.php'` are handled by
; `extract_php_imports` (it must read the left operand to count dirname levels).
