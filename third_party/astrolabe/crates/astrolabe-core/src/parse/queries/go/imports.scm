;; Go import specifiers (tree-sitter-go 0.25).
;;
;; `@import` — the import path with quotes already stripped: the capture is
;; the string's *content* node, not the literal, so `"fmt"` yields `fmt` and
;; `` `raw/path` `` yields `raw/path`. Aliased (`str "strings"`), blank
;; (`_ "embed"`) and dot imports all carry the same `path:` field.

(import_spec
  path: (interpreted_string_literal
    (interpreted_string_literal_content) @import))

(import_spec
  path: (raw_string_literal
    (raw_string_literal_content) @import))
