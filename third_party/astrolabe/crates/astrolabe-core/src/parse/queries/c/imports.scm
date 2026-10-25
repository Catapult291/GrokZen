;; C include specifiers (tree-sitter-c).
;;
;; `@import` — path text without surrounding quotes. `string_literal` captures
;; the content node (same convention as Go/JS). `system_lib_string` includes
;; angle brackets in the node text; production `unquote` strips them.

(preproc_include
  path: (string_literal
    (string_content) @import))

(preproc_include
  path: (system_lib_string) @import)
