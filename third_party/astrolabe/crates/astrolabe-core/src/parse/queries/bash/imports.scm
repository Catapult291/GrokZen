;; Shell / Bash import specifiers (tree-sitter-bash 0.25).
;;
;; `@import` — the first argument of `source FILE` / `. FILE` / `\\. FILE`
;; (nvm's alias-proof spelling), as written
;; (quotes are stripped by the consumer). Dynamic prefixes such as
;; `"$DIR/lib.sh"` or `"${BASH_SOURCE%/*}/x.sh"` are kept verbatim;
;; `resolvers/shell.rs` interprets them.

((command
  name: (command_name (word) @_cmd)
  .
  argument: [(word) (string) (raw_string) (concatenation)] @import)
  (#any-of? @_cmd "source" "." "\\."))
