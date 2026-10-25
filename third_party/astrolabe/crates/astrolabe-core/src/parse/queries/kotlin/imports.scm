;; Kotlin import specifiers (tree-sitter-kotlin-ng 1.1).
;;
;; `@import` — the dotted path as written, without `.*` or the alias:
;;   import a.b.C          -> a.b.C
;;   import a.b.*          -> a.b      (+ `@import.wildcard`)
;;   import a.b.C as D     -> a.b.C    (+ `@import.alias` = D)

(import
  (qualified_identifier) @import
  "*"? @import.wildcard
  (identifier)? @import.alias)
