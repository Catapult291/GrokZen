;; Shell / Bash call sites (tree-sitter-bash 0.25).
;;
;; Every simple command whose name is a literal word: function calls,
;; builtins and external programs alike. `@name` is the command word;
;; `$cmd args` (dynamic command names) is skipped.

(command
  name: (command_name
    (word) @name)) @reference.call
