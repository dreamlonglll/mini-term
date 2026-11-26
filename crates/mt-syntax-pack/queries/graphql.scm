; 来源: nvim-treesitter queries/graphql/highlights.scm (Apache-2.0)
; https://github.com/nvim-treesitter/nvim-treesitter
; 语法: bkegley/tree-sitter-graphql (crate tree-sitter-graphql 0.1)
; 捕获名是 nvim 口径,注册时经 zed_captures 改写

; Types
;------
(scalar_type_definition
  (name) @type)

(object_type_definition
  (name) @type)

(interface_type_definition
  (name) @type)

(union_type_definition
  (name) @type)

(enum_type_definition
  (name) @type)

(input_object_type_definition
  (name) @type)

(scalar_type_extension
  (name) @type)

(object_type_extension
  (name) @type)

(interface_type_extension
  (name) @type)

(union_type_extension
  (name) @type)

(enum_type_extension
  (name) @type)

(input_object_type_extension
  (name) @type)

(named_type
  (name) @type)

; Directives
;-----------
(directive_definition
  "@" @attribute
  (name) @attribute)

(directive) @attribute

; Properties
;-----------
(field
  (name) @property)

(field
  (alias
    (name) @property))

(field_definition
  (name) @property)

(object_value
  (object_field
    (name) @property))

(enum_value
  (name) @property)

; Variable Definitions and Arguments
;-----------------------------------
(operation_definition
  (name) @variable)

(fragment_name
  (name) @variable)

(input_fields_definition
  (input_value_definition
    (name) @variable.parameter))

(argument
  (name) @variable.parameter)

(arguments_definition
  (input_value_definition
    (name) @variable.parameter))

(variable_definition
  (variable) @variable.parameter)

(argument
  (value
    (variable) @variable))

; Constants
;----------
(string_value) @string

(int_value) @number

(float_value) @number.float

(boolean_value) @boolean

; Literals
;---------
(description
  (string_value) @string.documentation @spell)

(comment) @comment @spell

(directive_location
  (executable_directive_location) @type.builtin)

(directive_location
  (type_system_directive_location) @type.builtin)

; Keywords
;----------
[
  "query"
  "mutation"
  "subscription"
  "fragment"
  "scalar"
  "input"
  "extend"
  "directive"
  "schema"
  "on"
  "repeatable"
  "implements"
] @keyword

[
  "enum"
  "union"
  "type"
  "interface"
] @keyword.type

; Punctuation
;------------
[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

"=" @operator

"|" @punctuation.delimiter

"&" @punctuation.delimiter

":" @punctuation.delimiter

"..." @punctuation.special

"!" @punctuation.special
