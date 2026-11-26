; 来源: zed-extensions/csharp languages/csharp/highlights.scm (Apache-2.0)
; https://github.com/zed-extensions/csharp
; 语法: tree-sitter/tree-sitter-c-sharp (crate tree-sitter-c-sharp 0.23);捕获名已是 Zed 口径

(identifier) @variable

; Methods
(method_declaration
  name: (identifier) @function)

(_
  function: (identifier) @function)

(local_function_statement
  name: (identifier) @function)

; Types
(interface_declaration
  name: (identifier) @type)

(class_declaration
  name: (identifier) @type)

(enum_declaration
  name: (identifier) @type)

(struct_declaration
  (identifier) @type)

(record_declaration
  (identifier) @type)

; TODO: Let this be @module again once we support fallbacks
(namespace_declaration
  name: (identifier) @type)

(generic_name
  (identifier) @type)

(type_parameter
  (identifier) @property.definition)

(parameter
  type: (identifier) @type)

(type_argument_list
  (identifier) @type)

(as_expression
  right: (identifier) @type)

(is_expression
  right: (identifier) @type)

(constructor_declaration
  name: (identifier) @constructor)

(destructor_declaration
  name: (identifier) @constructor)

(_
  type: (identifier) @type)

(base_list
  (identifier) @type)

(predefined_type) @type.builtin

; Enum
(enum_member_declaration
  (identifier) @property.definition)

; Literals
[
  (real_literal)
  (integer_literal)
] @number

[
  (character_literal)
  (string_literal)
  (raw_string_literal)
  (verbatim_string_literal)
  (interpolated_string_expression)
  (interpolation_start)
  (interpolation_quote)
] @string

(escape_sequence) @string.escape

[
  (boolean_literal)
  (null_literal)
] @constant.builtin

; Comments
(comment) @comment

; Tokens
[
  ";"
  "."
  ","
] @punctuation.delimiter

[
  "--"
  "-"
  "-="
  "&"
  "&="
  "&&"
  "+"
  "++"
  "+="
  "<"
  "<="
  "<<"
  "<<="
  "="
  "=="
  "!"
  "!="
  "=>"
  ">"
  ">="
  ">>"
  ">>="
  ">>>"
  ">>>="
  "|"
  "|="
  "||"
  "?"
  "??"
  "??="
  "^"
  "^="
  "~"
  "*"
  "*="
  "/"
  "/="
  "%"
  "%="
  ":"
] @operator

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
  (interpolation_brace)
] @punctuation.bracket

; Keywords
[
  (modifier)
  "this"
  (implicit_type)
] @keyword

[
  "add"
  "alias"
  "as"
  "base"
  "break"
  "case"
  "catch"
  "checked"
  "class"
  "continue"
  "default"
  "delegate"
  "do"
  "else"
  "enum"
  "event"
  "explicit"
  "extern"
  "finally"
  "for"
  "foreach"
  "global"
  "goto"
  "if"
  "implicit"
  "interface"
  "is"
  "lock"
  "namespace"
  "notnull"
  "operator"
  "params"
  "return"
  "remove"
  "sizeof"
  "stackalloc"
  "static"
  "struct"
  "switch"
  "throw"
  "try"
  "typeof"
  "unchecked"
  "using"
  "while"
  "new"
  "await"
  "in"
  "yield"
  "get"
  "set"
  "when"
  "out"
  "ref"
  "from"
  "where"
  "select"
  "record"
  "init"
  "with"
  "let"
] @keyword

; pattern expression keywords
(negated_pattern
  "not" @keyword)

(and_pattern
  "and" @keyword)

(or_pattern
  "or" @keyword)

; scoped keyword
(scoped_type
  "scoped" @keyword)

; Attribute
(attribute
  name: (identifier) @attribute)

; Parameters
(parameter
  name: (identifier) @variable.parameter)

; Type constraints
(type_parameter_constraints_clause
  (identifier) @property.definition)

; Method calls
(invocation_expression
  (member_access_expression
    name: (identifier) @function))
