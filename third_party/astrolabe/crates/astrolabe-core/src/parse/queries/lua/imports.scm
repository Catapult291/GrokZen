;; Lua module imports (tree-sitter-lua 0.5).
;;
;; `@import` — the module string of `require "a.b"`, `require("a.b")`,
;; `require('a.b')`, `require [[a.b]]` (quotes removed by the extractor;
;; `string_content` is captured so long brackets need no unquoting).
;; Also `dofile("x.lua")` / `loadfile("x.lua")` (path-like specifiers).

(function_call
  name: (identifier) @_fn
  arguments: [
    (arguments
      .
      (string
        content: (string_content) @import))
    (string
      content: (string_content) @import)
  ]
  (#any-of? @_fn "require" "dofile" "loadfile"))
