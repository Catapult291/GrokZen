;; Swift imports (tree-sitter-swift 0.7).
;;
;; `@import` — the module path as written, without the import kind:
;;   import Foundation            -> Foundation
;;   @testable import Alamofire   -> Alamofire
;;   import struct Foo.Bar        -> Foo.Bar
;; Swift imports name modules (SwiftPM targets), not files.

(import_declaration
  (identifier) @import)
