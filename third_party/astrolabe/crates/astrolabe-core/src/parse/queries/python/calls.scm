;; Python call sites (tree-sitter-python 0.25).
;;
;; `@reference.call` on the call node (its start row is the call line),
;; `@name` on the callee's final identifier. Name-based only: `a.b.c()` yields
;; `c`. Consumers tag these `Confidence::Syntactic`.

(call
  function: [
    (identifier) @name
    (attribute
      attribute: (identifier) @name)
  ]) @reference.call
