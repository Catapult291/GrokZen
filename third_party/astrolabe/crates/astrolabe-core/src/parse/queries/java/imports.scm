;; Java import specifiers (tree-sitter-java 0.23).
;;
;; `@import` — the dotted path exactly as written, without the trailing `.*`:
;;   import a.b.C;            -> a.b.C
;;   import a.b.*;            -> a.b            (+ `@import.wildcard`)
;;   import static a.b.C.d;   -> a.b.C.d        (+ `@import.static`)
;;   import static a.b.C.*;   -> a.b.C          (+ both)
;; The optional captures are present on the same match when the token exists,
;; so a resolver can tell "type" from "package" and "member" imports.

(import_declaration
  "static"? @import.static
  [
    (scoped_identifier)
    (identifier)
  ] @import
  (asterisk)? @import.wildcard)
