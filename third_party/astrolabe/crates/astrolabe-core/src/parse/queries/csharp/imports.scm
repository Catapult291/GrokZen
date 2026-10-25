;; C# import specifiers (tree-sitter-c-sharp 0.23).
;;
;; `@import` is the name as written:
;;   using System.Text;           -> System.Text
;;   using static System.Math;    -> System.Math   (+ `@import.static`)
;;   using global::System.Linq;   -> global::System.Linq
;;   using IO = System.IO;        -> System.IO     (+ `@import.alias` = IO)
;;
;; `type` is a supertype, not a wrapper node, so an alias using's target is a
;; direct child (`qualified_name`, `identifier`, …) beside the `name:` field.
;; The plain-using pattern would also see that child; `#not-match?` drops any
;; directive whose text contains `=`.

(using_directive
  name: (identifier) @import.alias
  [
    (qualified_name)
    (generic_name)
    (alias_qualified_name)
    (predefined_type)
    (identifier)
  ] @import)

(using_directive
  "static"? @import.static
  [
    (identifier)
    (qualified_name)
    (generic_name)
    (alias_qualified_name)
  ] @import
  .
  ";") @_using
(#not-match? @_using "=")
