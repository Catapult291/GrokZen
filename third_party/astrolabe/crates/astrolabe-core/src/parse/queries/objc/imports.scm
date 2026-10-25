;; Objective-C imports: `#import` / `#include` (both are `preproc_include`
;; in tree-sitter-objc) and `@import Module.Sub;`.
;;
;; `@import` — path text; `system_lib_string` keeps its angle brackets, which
;; production `unquote` strips (same as C).

(preproc_include
  path: (string_literal
    (string_content) @import))

(preproc_include
  path: (system_lib_string) @import)

; `@import UIKit.UIView;` -> `UIKit` (module granularity).
(module_import
  .
  path: (identifier) @import)
