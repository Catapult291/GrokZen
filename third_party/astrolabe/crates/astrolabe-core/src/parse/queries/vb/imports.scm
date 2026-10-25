;; Visual Basic .NET Imports (tree-sitter-vb-dotnet 0.1).
;;
;; `Imports System.Text` and `Imports A, B` each contribute a namespace_name.
;; The grammar has no alias form; the resolver still accepts `alias:` specs.

(imports_statement
  (namespace_name) @import)
