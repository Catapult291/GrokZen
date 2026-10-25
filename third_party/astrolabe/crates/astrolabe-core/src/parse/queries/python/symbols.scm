;; Python symbol definitions (tree-sitter-python 0.25).
;;
;; Conventions: `@definition.<kind>` on the defining node, `@name` on its
;; identifier. Captures whose name starts with `_` are pattern-internal and must
;; be ignored by consumers. Based on upstream `queries/tags.scm`, extended so
;; that methods and functions never double-report and decorated definitions are
;; covered (decorators live in a `decorated_definition` wrapper, so a pattern
;; that anchors on the parent must spell the wrapper out).

;; ---------------------------------------------------------------- classes
;; Matches at any depth, decorated or not.
(class_definition
  name: (identifier) @name) @definition.class

;; ---------------------------------------------------------------- methods
;; A method is a function whose enclosing block is a class body.
(class_definition
  body: (block
    (function_definition
      name: (identifier) @name) @definition.method))

(class_definition
  body: (block
    (decorated_definition
      (function_definition
        name: (identifier) @name) @definition.method)))

;; -------------------------------------------------------------- functions
;; Module level.
(module
  (function_definition
    name: (identifier) @name) @definition.function)

(module
  (decorated_definition
    (function_definition
      name: (identifier) @name) @definition.function))

;; Any other block: function bodies (nested defs), `if`/`elif`/`else`, `try`/
;; `except`/`finally`, `with`, `for`, `while`, `match` arms. The block's parent
;; is captured and rejected when it is a class, which is what keeps these
;; disjoint from the method patterns above. Every non-class block owner starts
;; with a different keyword (`def`, `async`, `if`, `try`, ...).
;; NB: `(_ (block ...))` is a wildcard *parent* with a child; `((_) (block ...))`
;; would be two sibling patterns and bind `@_scope` to the wrong node.
((_
  (block
    (function_definition
      name: (identifier) @name) @definition.function)) @_scope
  (#not-match? @_scope "^class\\b"))

((_
  (block
    (decorated_definition
      (function_definition
        name: (identifier) @name) @definition.function))) @_scope
  (#not-match? @_scope "^class\\b"))

;; -------------------------------------------------------- module bindings
;; Module-level `NAME = ...` / `NAME: T = ...`. PEP 8 constants (`MAX`,
;; `HTTP_OK`) stay Const; any other binding is a Variable. The two patterns
;; are mutually exclusive via `#match?` / `#not-match?` on the same
;; identifier, so a node never carries both kinds. Class attributes and
;; locals are skipped here.
(module
  (expression_statement
    ((assignment
      left: (identifier) @name) @definition.const
      (#match? @name "^[A-Z][A-Z0-9_]*$"))))

(module
  (expression_statement
    ((assignment
      left: (identifier) @name) @definition.variable
      (#not-match? @name "^[A-Z][A-Z0-9_]*$"))))

;; ---------------------------------------------------------- class fields
;; Assignments in a class body: plain attributes, annotated attributes
;; (`x: int`), and dataclass fields (`x: int = 0`). Decorators wrap the
;; class, not the assignment, so these sit directly in the class block.
(class_definition
  body: (block
    (expression_statement
      (assignment
        left: (identifier) @name) @definition.field)))
