;; Python import specifiers (tree-sitter-python 0.25).
;;
;; `@import` — the module specifier exactly as written, one per statement /
;;             per `import a, b` entry. Relative imports keep their leading
;;             dots (`.`, `..pkg`) so the resolver can count levels.
;; Optional refinement, emitted as separate matches:
;; `@import.module` + `@import.name` — one pair per imported name in a
;;             `from X import a, b` statement, so a resolver can probe
;;             `X.a` / `X.b` as submodules (`from . import views`).

;; import a.b            -> a.b
;; import a.b as c, d    -> a.b, d
(import_statement
  name: (dotted_name) @import)

(import_statement
  name: (aliased_import
    name: (dotted_name) @import))

;; from a.b import c     -> a.b
;; from . import c       -> .
;; from ..a import b     -> ..a
;; from .x import *      -> .x
(import_from_statement
  module_name: [(dotted_name) (relative_import)] @import)

;; (module, name) pairs for submodule probing.
(import_from_statement
  module_name: [(dotted_name) (relative_import)] @import.module
  name: [
    (dotted_name) @import.name
    (aliased_import
      name: (dotted_name) @import.name)
  ])
