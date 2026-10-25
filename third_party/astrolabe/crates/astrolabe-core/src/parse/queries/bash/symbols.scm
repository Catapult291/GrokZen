;; Shell / Bash symbol definitions (tree-sitter-bash 0.25).
;;
;; Conventions: `@definition.<kind>` on the defining node, `@name` on its
;; identifier. `_`-prefixed captures are pattern-internal.
;;
;; Declarations a shell file exposes to whoever sources it:
;;   * functions (`f() {}`, `function f {}`, `function f() {}`) at any depth —
;;     a function defined inside another becomes global once the outer runs;
;;   * file-scope variables: plain `NAME=value` and `export` / `declare` /
;;     `typeset` / `readonly` assignments that are direct children of the file.
;;     `parse/shell.rs` turns `readonly` / `declare -r` into `Const`.
;; `local` is function-scoped and never a symbol; assignments inside function
;; bodies, loops and conditionals are runtime state, not declarations.

(function_definition
  name: (word) @name) @definition.function

(program
  (variable_assignment
    name: (variable_name) @name) @definition.variable)

((program
  (declaration_command
    (variable_assignment
      name: (variable_name) @name) @definition.variable) @_decl)
  (#not-match? @_decl "^local\\b"))
