; Ruby imports: `require`, `require_relative`, `load` (first string
; argument) and `autoload :Name, "path"` (second argument). The resolver
; tries load-path roots (`lib/` …) first, then the requiring file's dir.
(call
  !receiver
  method: (identifier) @_m
  arguments: (argument_list . (string . (string_content) @import .))
  (#match? @_m "^(require|require_relative|load)$"))

(call
  !receiver
  method: (identifier) @_m
  arguments: (argument_list (simple_symbol) . (string . (string_content) @import .))
  (#eq? @_m "autoload"))
