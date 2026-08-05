from __future__ import annotations

from .base import *


@proto_enum(
    {
        "package": "scip",
        "name": "ProtocolVersion",
        "parent": None,
        "description": None,
        "allow_alias": False,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {"name": "UnspecifiedProtocolVersion", "number": 0, "deprecated": False}
        ],
    }
)
class ProtocolVersion(IntEnum):
    UnspecifiedProtocolVersion = 0


@proto_enum(
    {
        "package": "scip",
        "name": "TextEncoding",
        "parent": None,
        "description": None,
        "allow_alias": False,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {"name": "UnspecifiedTextEncoding", "number": 0, "deprecated": False},
            {"name": "UTF8", "number": 1, "deprecated": False},
            {"name": "UTF16", "number": 2, "deprecated": False},
        ],
    }
)
class TextEncoding(IntEnum):
    UnspecifiedTextEncoding = 0
    UTF8 = 1
    UTF16 = 2


@proto_enum(
    {
        "package": "scip",
        "name": "PositionEncoding",
        "parent": None,
        "description": "Encoding used to interpret the 'character' value in source ranges.",
        "allow_alias": False,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {
                "name": "UnspecifiedPositionEncoding",
                "number": 0,
                "description": "Default value. This value should not be used by new SCIP indexers\n"
                " so that a consumer can process the SCIP index without ambiguity.",
                "deprecated": False,
            },
            {
                "name": "UTF8CodeUnitOffsetFromLineStart",
                "number": 1,
                "description": "The 'character' value is interpreted as an offset in terms\n"
                " of UTF-8 code units (i.e. bytes).\n"
                "\n"
                ' Example: For the string "🚀 Woo" in UTF-8, the bytes are\n'
                " [240, 159, 154, 128, 32, 87, 111, 111], so the offset for 'W'\n"
                " would be 5.",
                "deprecated": False,
            },
            {
                "name": "UTF16CodeUnitOffsetFromLineStart",
                "number": 2,
                "description": "The 'character' value is interpreted as an offset in terms\n"
                " of UTF-16 code units (each is 2 bytes).\n"
                "\n"
                ' Example: For the string "🚀 Woo", the UTF-16 code units are\n'
                " ['\\ud83d', '\\ude80', ' ', 'W', 'o', 'o'], so the offset for 'W'\n"
                " would be 3.",
                "deprecated": False,
            },
            {
                "name": "UTF32CodeUnitOffsetFromLineStart",
                "number": 3,
                "description": "The 'character' value is interpreted as an offset in terms\n"
                " of UTF-32 code units (each is 4 bytes).\n"
                "\n"
                ' Example: For the string "🚀 Woo", the UTF-32 code units are\n'
                " ['🚀', ' ', 'W', 'o', 'o'], so the offset for 'W' would be 2.",
                "deprecated": False,
            },
        ],
    }
)
class PositionEncoding(IntEnum):
    """Encoding used to interpret the 'character' value in source ranges."""

    UnspecifiedPositionEncoding = 0
    UTF8CodeUnitOffsetFromLineStart = 1
    UTF16CodeUnitOffsetFromLineStart = 2
    UTF32CodeUnitOffsetFromLineStart = 3


@proto_enum(
    {
        "package": "scip",
        "name": "SymbolRole",
        "parent": None,
        "description": 'SymbolRole declares what "role" a symbol has in an occurrence. A role is\n'
        " encoded as a bitset where each bit represents a different role. For example,\n"
        " to determine if the `Import` role is set, test whether the second bit of the\n"
        " enum value is defined. In pseudocode, this can be implemented with the\n"
        " logic: `const isImportRole = (role.value & SymbolRole.Import.value) > 0`.",
        "allow_alias": False,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {
                "name": "UnspecifiedSymbolRole",
                "number": 0,
                "description": "This case is not meant to be used; it only exists to avoid an error\n"
                " from the Protobuf code generator.",
                "deprecated": False,
            },
            {
                "name": "Definition",
                "number": 1,
                "description": "Is the symbol defined here? If not, then this is a symbol reference.",
                "deprecated": False,
            },
            {
                "name": "Import",
                "number": 2,
                "description": "Is the symbol imported here?",
                "deprecated": False,
            },
            {
                "name": "WriteAccess",
                "number": 4,
                "description": "Is the symbol written here?",
                "deprecated": False,
            },
            {
                "name": "ReadAccess",
                "number": 8,
                "description": "Is the symbol read here?",
                "deprecated": False,
            },
            {
                "name": "Generated",
                "number": 16,
                "description": "Is the symbol in generated code?",
                "deprecated": False,
            },
            {
                "name": "Test",
                "number": 32,
                "description": "Is the symbol in test code?",
                "deprecated": False,
            },
            {
                "name": "ForwardDefinition",
                "number": 64,
                "description": "Is this a signature for a symbol that is defined elsewhere?\n"
                "\n"
                " Applies to forward declarations for languages like C, C++\n"
                " and Objective-C, as well as `val` declarations in interface\n"
                " files in languages like SML and OCaml.",
                "deprecated": False,
            },
        ],
    }
)
class SymbolRole(IntEnum):
    """SymbolRole declares what "role" a symbol has in an occurrence. A role is
    encoded as a bitset where each bit represents a different role. For example,
    to determine if the `Import` role is set, test whether the second bit of the
    enum value is defined. In pseudocode, this can be implemented with the
    logic: `const isImportRole = (role.value & SymbolRole.Import.value) > 0`."""

    UnspecifiedSymbolRole = 0
    Definition = 1
    Import = 2
    WriteAccess = 4
    ReadAccess = 8
    Generated = 16
    Test = 32
    ForwardDefinition = 64


@proto_enum(
    {
        "package": "scip",
        "name": "SyntaxKind",
        "parent": None,
        "description": None,
        "allow_alias": True,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {"name": "UnspecifiedSyntaxKind", "number": 0, "deprecated": False},
            {
                "name": "Comment",
                "number": 1,
                "description": "Comment, including comment markers and text",
                "deprecated": False,
            },
            {
                "name": "PunctuationDelimiter",
                "number": 2,
                "description": "`;` `.` `,`",
                "deprecated": False,
            },
            {
                "name": "PunctuationBracket",
                "number": 3,
                "description": "(), {}, [] when used syntactically",
                "deprecated": False,
            },
            {
                "name": "Keyword",
                "number": 4,
                "description": "`if`, `else`, `return`, `class`, etc.",
                "deprecated": False,
            },
            {"name": "IdentifierKeyword", "number": 4, "deprecated": True},
            {
                "name": "IdentifierOperator",
                "number": 5,
                "description": "`+`, `*`, etc.",
                "deprecated": False,
            },
            {
                "name": "Identifier",
                "number": 6,
                "description": "non-specific catch-all for any identifier not better described elsewhere",
                "deprecated": False,
            },
            {
                "name": "IdentifierBuiltin",
                "number": 7,
                "description": "Identifiers builtin to the language: `min`, `print` in Python.",
                "deprecated": False,
            },
            {
                "name": "IdentifierNull",
                "number": 8,
                "description": "Identifiers representing `null`-like values: `None` in Python, `nil` in Go.",
                "deprecated": False,
            },
            {
                "name": "IdentifierConstant",
                "number": 9,
                "description": '`xyz` in `const xyz = "hello"`',
                "deprecated": False,
            },
            {
                "name": "IdentifierMutableGlobal",
                "number": 10,
                "description": '`var X = "hello"` in Go',
                "deprecated": False,
            },
            {
                "name": "IdentifierParameter",
                "number": 11,
                "description": "Parameter definition and references",
                "deprecated": False,
            },
            {
                "name": "IdentifierLocal",
                "number": 12,
                "description": "Identifiers for variable definitions and references within a local scope",
                "deprecated": False,
            },
            {
                "name": "IdentifierShadowed",
                "number": 13,
                "description": "Identifiers that shadow other identifiers in an outer scope",
                "deprecated": False,
            },
            {
                "name": "IdentifierNamespace",
                "number": 14,
                "description": "Identifier representing a unit of code abstraction and/or namespacing.\n"
                "\n"
                " NOTE: This corresponds to a package in Go and JVM languages,\n"
                " and a module in languages like Python and JavaScript.",
                "deprecated": False,
            },
            {"name": "IdentifierModule", "number": 14, "deprecated": True},
            {
                "name": "IdentifierFunction",
                "number": 15,
                "description": "Function references, including calls",
                "deprecated": False,
            },
            {
                "name": "IdentifierFunctionDefinition",
                "number": 16,
                "description": "Function definition only",
                "deprecated": False,
            },
            {
                "name": "IdentifierMacro",
                "number": 17,
                "description": "Macro references, including invocations",
                "deprecated": False,
            },
            {
                "name": "IdentifierMacroDefinition",
                "number": 18,
                "description": "Macro definition only",
                "deprecated": False,
            },
            {
                "name": "IdentifierType",
                "number": 19,
                "description": "non-builtin types",
                "deprecated": False,
            },
            {
                "name": "IdentifierBuiltinType",
                "number": 20,
                "description": "builtin types only, such as `str` for Python or `int` in Go",
                "deprecated": False,
            },
            {
                "name": "IdentifierAttribute",
                "number": 21,
                "description": "Python decorators, c-like __attribute__",
                "deprecated": False,
            },
            {
                "name": "RegexEscape",
                "number": 22,
                "description": "`\\b`",
                "deprecated": False,
            },
            {
                "name": "RegexRepeated",
                "number": 23,
                "description": "`*`, `+`",
                "deprecated": False,
            },
            {
                "name": "RegexWildcard",
                "number": 24,
                "description": "`.`",
                "deprecated": False,
            },
            {
                "name": "RegexDelimiter",
                "number": 25,
                "description": "`(`, `)`, `[`, `]`",
                "deprecated": False,
            },
            {
                "name": "RegexJoin",
                "number": 26,
                "description": "`|`, `-`",
                "deprecated": False,
            },
            {
                "name": "StringLiteral",
                "number": 27,
                "description": 'Literal strings: "Hello, world!"',
                "deprecated": False,
            },
            {
                "name": "StringLiteralEscape",
                "number": 28,
                "description": 'non-regex escapes: "\\t", "\\n"',
                "deprecated": False,
            },
            {
                "name": "StringLiteralSpecial",
                "number": 29,
                "description": "datetimes within strings, special words within a string, `{}` in format strings",
                "deprecated": False,
            },
            {
                "name": "StringLiteralKey",
                "number": 30,
                "description": '"key" in { "key": "value" }, useful for example in JSON',
                "deprecated": False,
            },
            {
                "name": "CharacterLiteral",
                "number": 31,
                "description": "'c' or similar, in languages that differentiate strings and characters",
                "deprecated": False,
            },
            {
                "name": "NumericLiteral",
                "number": 32,
                "description": "Literal numbers, both floats and integers",
                "deprecated": False,
            },
            {
                "name": "BooleanLiteral",
                "number": 33,
                "description": "`true`, `false`",
                "deprecated": False,
            },
            {
                "name": "Tag",
                "number": 34,
                "description": "Used for XML-like tags",
                "deprecated": False,
            },
            {
                "name": "TagAttribute",
                "number": 35,
                "description": "Attribute name in XML-like tags",
                "deprecated": False,
            },
            {
                "name": "TagDelimiter",
                "number": 36,
                "description": "Delimiters for XML-like tags",
                "deprecated": False,
            },
        ],
    }
)
class SyntaxKind(IntEnum):
    UnspecifiedSyntaxKind = 0
    Comment = 1
    PunctuationDelimiter = 2
    PunctuationBracket = 3
    Keyword = 4
    IdentifierKeyword = 4
    IdentifierOperator = 5
    Identifier = 6
    IdentifierBuiltin = 7
    IdentifierNull = 8
    IdentifierConstant = 9
    IdentifierMutableGlobal = 10
    IdentifierParameter = 11
    IdentifierLocal = 12
    IdentifierShadowed = 13
    IdentifierNamespace = 14
    IdentifierModule = 14
    IdentifierFunction = 15
    IdentifierFunctionDefinition = 16
    IdentifierMacro = 17
    IdentifierMacroDefinition = 18
    IdentifierType = 19
    IdentifierBuiltinType = 20
    IdentifierAttribute = 21
    RegexEscape = 22
    RegexRepeated = 23
    RegexWildcard = 24
    RegexDelimiter = 25
    RegexJoin = 26
    StringLiteral = 27
    StringLiteralEscape = 28
    StringLiteralSpecial = 29
    StringLiteralKey = 30
    CharacterLiteral = 31
    NumericLiteral = 32
    BooleanLiteral = 33
    Tag = 34
    TagAttribute = 35
    TagDelimiter = 36


@proto_enum(
    {
        "package": "scip",
        "name": "Severity",
        "parent": None,
        "description": None,
        "allow_alias": False,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {"name": "UnspecifiedSeverity", "number": 0, "deprecated": False},
            {"name": "Error", "number": 1, "deprecated": False},
            {"name": "Warning", "number": 2, "deprecated": False},
            {"name": "Information", "number": 3, "deprecated": False},
            {"name": "Hint", "number": 4, "deprecated": False},
        ],
    }
)
class Severity(IntEnum):
    UnspecifiedSeverity = 0
    Error = 1
    Warning = 2
    Information = 3
    Hint = 4


@proto_enum(
    {
        "package": "scip",
        "name": "DiagnosticTag",
        "parent": None,
        "description": None,
        "allow_alias": False,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {"name": "UnspecifiedDiagnosticTag", "number": 0, "deprecated": False},
            {"name": "Unnecessary", "number": 1, "deprecated": False},
            {"name": "Deprecated", "number": 2, "deprecated": False},
        ],
    }
)
class DiagnosticTag(IntEnum):
    UnspecifiedDiagnosticTag = 0
    Unnecessary = 1
    Deprecated = 2


@proto_enum(
    {
        "package": "scip",
        "name": "Language",
        "parent": None,
        "description": "Language standardises names of common programming languages that can be used\n"
        " for the `Document.language` field. The primary purpose of this enum is to\n"
        " prevent a situation where we have a single programming language ends up with\n"
        " multiple string representations. For example, the C++ language uses the name\n"
        ' "CPP" in this enum and other names such as "cpp" are incompatible.\n'
        " Feel free to send a pull-request to add missing programming languages.",
        "allow_alias": False,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {"name": "UnspecifiedLanguage", "number": 0, "deprecated": False},
            {"name": "ABAP", "number": 60, "deprecated": False},
            {"name": "Apex", "number": 96, "deprecated": False},
            {"name": "APL", "number": 49, "deprecated": False},
            {"name": "Ada", "number": 39, "deprecated": False},
            {"name": "Agda", "number": 45, "deprecated": False},
            {"name": "AsciiDoc", "number": 86, "deprecated": False},
            {"name": "Assembly", "number": 58, "deprecated": False},
            {"name": "Awk", "number": 66, "deprecated": False},
            {"name": "Bat", "number": 68, "deprecated": False},
            {"name": "BibTeX", "number": 81, "deprecated": False},
            {"name": "C", "number": 34, "deprecated": False},
            {"name": "COBOL", "number": 59, "deprecated": False},
            {
                "name": "CPP",
                "number": 35,
                "description": 'C++ (the name "CPP" was chosen for consistency with LSP)',
                "deprecated": False,
            },
            {"name": "CSS", "number": 26, "deprecated": False},
            {"name": "CSharp", "number": 1, "deprecated": False},
            {"name": "Clojure", "number": 8, "deprecated": False},
            {"name": "Coffeescript", "number": 21, "deprecated": False},
            {"name": "CommonLisp", "number": 9, "deprecated": False},
            {"name": "Coq", "number": 47, "deprecated": False},
            {"name": "CUDA", "number": 97, "deprecated": False},
            {"name": "Dart", "number": 3, "deprecated": False},
            {"name": "Delphi", "number": 57, "deprecated": False},
            {"name": "Diff", "number": 88, "deprecated": False},
            {"name": "Dockerfile", "number": 80, "deprecated": False},
            {"name": "Dyalog", "number": 50, "deprecated": False},
            {"name": "Elixir", "number": 17, "deprecated": False},
            {"name": "Erlang", "number": 18, "deprecated": False},
            {"name": "FSharp", "number": 42, "deprecated": False},
            {"name": "Fish", "number": 65, "deprecated": False},
            {"name": "Flow", "number": 24, "deprecated": False},
            {"name": "Fortran", "number": 56, "deprecated": False},
            {"name": "Git_Commit", "number": 91, "deprecated": False},
            {"name": "Git_Config", "number": 89, "deprecated": False},
            {"name": "Git_Rebase", "number": 92, "deprecated": False},
            {"name": "Go", "number": 33, "deprecated": False},
            {"name": "GraphQL", "number": 98, "deprecated": False},
            {"name": "Groovy", "number": 7, "deprecated": False},
            {"name": "HTML", "number": 30, "deprecated": False},
            {"name": "Hack", "number": 20, "deprecated": False},
            {"name": "Handlebars", "number": 90, "deprecated": False},
            {"name": "Haskell", "number": 44, "deprecated": False},
            {"name": "Idris", "number": 46, "deprecated": False},
            {"name": "Ini", "number": 72, "deprecated": False},
            {"name": "J", "number": 51, "deprecated": False},
            {"name": "JSON", "number": 75, "deprecated": False},
            {"name": "Java", "number": 6, "deprecated": False},
            {"name": "JavaScript", "number": 22, "deprecated": False},
            {"name": "JavaScriptReact", "number": 93, "deprecated": False},
            {"name": "Jsonnet", "number": 76, "deprecated": False},
            {"name": "Julia", "number": 55, "deprecated": False},
            {"name": "Justfile", "number": 109, "deprecated": False},
            {"name": "Kotlin", "number": 4, "deprecated": False},
            {"name": "LaTeX", "number": 83, "deprecated": False},
            {"name": "Lean", "number": 48, "deprecated": False},
            {"name": "Less", "number": 27, "deprecated": False},
            {"name": "Lua", "number": 12, "deprecated": False},
            {"name": "Luau", "number": 108, "deprecated": False},
            {"name": "Makefile", "number": 79, "deprecated": False},
            {"name": "Markdown", "number": 84, "deprecated": False},
            {"name": "Matlab", "number": 52, "deprecated": False},
            {
                "name": "Nickel",
                "number": 110,
                "description": "https://nickel-lang.org/",
                "deprecated": False,
            },
            {"name": "Nix", "number": 77, "deprecated": False},
            {"name": "OCaml", "number": 41, "deprecated": False},
            {"name": "Objective_C", "number": 36, "deprecated": False},
            {"name": "Objective_CPP", "number": 37, "deprecated": False},
            {"name": "Pascal", "number": 99, "deprecated": False},
            {"name": "PHP", "number": 19, "deprecated": False},
            {"name": "PLSQL", "number": 70, "deprecated": False},
            {"name": "Perl", "number": 13, "deprecated": False},
            {"name": "PowerShell", "number": 67, "deprecated": False},
            {"name": "Prolog", "number": 71, "deprecated": False},
            {"name": "Protobuf", "number": 100, "deprecated": False},
            {"name": "Python", "number": 15, "deprecated": False},
            {"name": "R", "number": 54, "deprecated": False},
            {"name": "Racket", "number": 11, "deprecated": False},
            {"name": "Raku", "number": 14, "deprecated": False},
            {"name": "Razor", "number": 62, "deprecated": False},
            {
                "name": "Repro",
                "number": 102,
                "description": "Internal language for testing SCIP",
                "deprecated": False,
            },
            {"name": "ReST", "number": 85, "deprecated": False},
            {"name": "Ruby", "number": 16, "deprecated": False},
            {"name": "Rust", "number": 40, "deprecated": False},
            {"name": "SAS", "number": 61, "deprecated": False},
            {"name": "SCSS", "number": 29, "deprecated": False},
            {"name": "SML", "number": 43, "deprecated": False},
            {"name": "SQL", "number": 69, "deprecated": False},
            {"name": "Sass", "number": 28, "deprecated": False},
            {"name": "Scala", "number": 5, "deprecated": False},
            {"name": "Scheme", "number": 10, "deprecated": False},
            {
                "name": "ShellScript",
                "number": 64,
                "description": "Bash",
                "deprecated": False,
            },
            {"name": "Skylark", "number": 78, "deprecated": False},
            {"name": "Slang", "number": 107, "deprecated": False},
            {"name": "Solidity", "number": 95, "deprecated": False},
            {"name": "Svelte", "number": 106, "deprecated": False},
            {"name": "Swift", "number": 2, "deprecated": False},
            {"name": "Tcl", "number": 101, "deprecated": False},
            {"name": "TOML", "number": 73, "deprecated": False},
            {"name": "TeX", "number": 82, "deprecated": False},
            {"name": "Thrift", "number": 103, "deprecated": False},
            {"name": "TypeScript", "number": 23, "deprecated": False},
            {"name": "TypeScriptReact", "number": 94, "deprecated": False},
            {"name": "Verilog", "number": 104, "deprecated": False},
            {"name": "VHDL", "number": 105, "deprecated": False},
            {"name": "VisualBasic", "number": 63, "deprecated": False},
            {"name": "Vue", "number": 25, "deprecated": False},
            {"name": "Wolfram", "number": 53, "deprecated": False},
            {"name": "XML", "number": 31, "deprecated": False},
            {"name": "XSL", "number": 32, "deprecated": False},
            {"name": "YAML", "number": 74, "deprecated": False},
            {
                "name": "Zig",
                "number": 38,
                "description": "NextLanguage = 111;\n"
                " Steps add a new language:\n"
                ' 1. Copy-paste the "NextLanguage = N" line above\n'
                ' 2. Increment "NextLanguage = N" to "NextLanguage = N+1"\n'
                ' 3. Replace "NextLanguage = N" with the name of the new language.\n'
                " 4. Move the new language to the correct line above using alphabetical order\n"
                " 5. (optional) Add a brief comment behind the language if the name is not "
                "self-explanatory",
                "deprecated": False,
            },
        ],
    }
)
class Language(IntEnum):
    """Language standardises names of common programming languages that can be used
    for the `Document.language` field. The primary purpose of this enum is to
    prevent a situation where we have a single programming language ends up with
    multiple string representations. For example, the C++ language uses the name
    "CPP" in this enum and other names such as "cpp" are incompatible.
    Feel free to send a pull-request to add missing programming languages."""

    UnspecifiedLanguage = 0
    ABAP = 60
    Apex = 96
    APL = 49
    Ada = 39
    Agda = 45
    AsciiDoc = 86
    Assembly = 58
    Awk = 66
    Bat = 68
    BibTeX = 81
    C = 34
    COBOL = 59
    CPP = 35
    CSS = 26
    CSharp = 1
    Clojure = 8
    Coffeescript = 21
    CommonLisp = 9
    Coq = 47
    CUDA = 97
    Dart = 3
    Delphi = 57
    Diff = 88
    Dockerfile = 80
    Dyalog = 50
    Elixir = 17
    Erlang = 18
    FSharp = 42
    Fish = 65
    Flow = 24
    Fortran = 56
    Git_Commit = 91
    Git_Config = 89
    Git_Rebase = 92
    Go = 33
    GraphQL = 98
    Groovy = 7
    HTML = 30
    Hack = 20
    Handlebars = 90
    Haskell = 44
    Idris = 46
    Ini = 72
    J = 51
    JSON = 75
    Java = 6
    JavaScript = 22
    JavaScriptReact = 93
    Jsonnet = 76
    Julia = 55
    Justfile = 109
    Kotlin = 4
    LaTeX = 83
    Lean = 48
    Less = 27
    Lua = 12
    Luau = 108
    Makefile = 79
    Markdown = 84
    Matlab = 52
    Nickel = 110
    Nix = 77
    OCaml = 41
    Objective_C = 36
    Objective_CPP = 37
    Pascal = 99
    PHP = 19
    PLSQL = 70
    Perl = 13
    PowerShell = 67
    Prolog = 71
    Protobuf = 100
    Python = 15
    R = 54
    Racket = 11
    Raku = 14
    Razor = 62
    Repro = 102
    ReST = 85
    Ruby = 16
    Rust = 40
    SAS = 61
    SCSS = 29
    SML = 43
    SQL = 69
    Sass = 28
    Scala = 5
    Scheme = 10
    ShellScript = 64
    Skylark = 78
    Slang = 107
    Solidity = 95
    Svelte = 106
    Swift = 2
    Tcl = 101
    TOML = 73
    TeX = 82
    Thrift = 103
    TypeScript = 23
    TypeScriptReact = 94
    Verilog = 104
    VHDL = 105
    VisualBasic = 63
    Vue = 25
    Wolfram = 53
    XML = 31
    XSL = 32
    YAML = 74
    Zig = 38


@proto_message(
    {
        "package": "scip",
        "name": "Index",
        "parent": None,
        "description": "Index represents a complete SCIP index for a workspace this is rooted at a\n"
        " single directory. An Index message payload can have a large memory footprint\n"
        " and it's therefore recommended to emit and consume an Index payload one field\n"
        " value at a time. To permit streaming consumption of an Index payload, the\n"
        " `metadata` field must appear at the start of the stream and must only appear\n"
        " once in the stream. Other field values may appear in any order.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Index(ProtoModel):
    """Index represents a complete SCIP index for a workspace this is rooted at a
    single directory. An Index message payload can have a large memory footprint
    and it's therefore recommended to emit and consume an Index payload one field
    value at a time. To permit streaming consumption of an Index payload, the
    `metadata` field must appear at the start of the stream and must only appear
    once in the stream. Other field values may appear in any order."""

    metadata: Metadata = proto_field(
        default=...,
        spec={
            "name": "metadata",
            "number": 1,
            "type": "scip.Metadata",
            "description": "Metadata about this index.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    documents: list[Document] = proto_field(
        default=...,
        spec={
            "name": "documents",
            "number": 2,
            "type": "scip.Document",
            "description": "Documents that belong to this index.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    external_symbols: list[SymbolInformation] = proto_field(
        default=...,
        spec={
            "name": "external_symbols",
            "number": 3,
            "type": "scip.SymbolInformation",
            "description": "(optional) Symbols that are referenced from this index but are defined in\n"
            " an external package (a separate `Index` message). Leave this field empty\n"
            " if you assume the external package will get indexed separately. If the\n"
            " external package won't get indexed for some reason then you can use this\n"
            " field to provide hover documentation for those external symbols.\n"
            "\n"
            "IMPORTANT: When adding a new field to `Index` here, add a matching\n"
            " function in `IndexVisitor` and update `ParseStreaming`.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "scip",
        "name": "Metadata",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Metadata(ProtoModel):
    version: ProtocolVersion = proto_field(
        default=...,
        spec={
            "name": "version",
            "number": 1,
            "type": "scip.ProtocolVersion",
            "description": "Which version of this protocol was used to generate this index?",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    tool_info: ToolInfo = proto_field(
        default=...,
        spec={
            "name": "tool_info",
            "number": 2,
            "type": "scip.ToolInfo",
            "description": "Information about the tool that produced this index.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    project_root: str = proto_field(
        default=...,
        spec={
            "name": "project_root",
            "number": 3,
            "type": "string",
            "description": "URI-encoded absolute path to the root directory of this index. All\n"
            " documents in this index must appear in a subdirectory of this root\n"
            " directory.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    text_document_encoding: TextEncoding = proto_field(
        default=...,
        spec={
            "name": "text_document_encoding",
            "number": 4,
            "type": "scip.TextEncoding",
            "description": "Text encoding of the source files on disk that are referenced from\n"
            " `Document.relative_path`. This value is unrelated to the `Document.text`\n"
            " field, which is a Protobuf string and hence must be UTF-8 encoded.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "scip",
        "name": "ToolInfo",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class ToolInfo(ProtoModel):
    name: str = proto_field(
        default=...,
        spec={
            "name": "name",
            "number": 1,
            "type": "string",
            "description": "Name of the indexer that produced this index.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    version: str = proto_field(
        default=...,
        spec={
            "name": "version",
            "number": 2,
            "type": "string",
            "description": "Version of the indexer that produced this index.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    arguments: list[str] = proto_field(
        default=...,
        spec={
            "name": "arguments",
            "number": 3,
            "type": "string",
            "description": "Command-line arguments that were used to invoke this indexer.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "scip",
        "name": "Document",
        "parent": None,
        "description": "Document defines the metadata about a source file on disk.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Document(ProtoModel):
    """Document defines the metadata about a source file on disk."""

    language: str = proto_field(
        default=...,
        spec={
            "name": "language",
            "number": 4,
            "type": "string",
            "description": "The string ID for the programming language this file is written in.\n"
            " The `Language` enum contains the names of most common programming languages.\n"
            " This field is typed as a string to permit any programming language, including\n"
            " ones that are not specified by the `Language` enum.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    relative_path: str = proto_field(
        default=...,
        spec={
            "name": "relative_path",
            "number": 1,
            "type": "string",
            "description": "(Required) Unique path to the text document.\n"
            "\n"
            " 1. The path must be relative to the directory supplied in the associated\n"
            "    `Metadata.project_root`.\n"
            " 2. The path must not begin with a leading '/'.\n"
            " 3. The path must point to a regular file, not a symbolic link.\n"
            " 4. The path must use '/' as the separator, including on Windows.\n"
            " 5. The path must be canonical; it cannot include empty components ('//'),\n"
            "    or '.' or '..'.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    occurrences: list[Occurrence] = proto_field(
        default=...,
        spec={
            "name": "occurrences",
            "number": 2,
            "type": "scip.Occurrence",
            "description": "Occurrences that appear in this file.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    symbols: list[SymbolInformation] = proto_field(
        default=...,
        spec={
            "name": "symbols",
            "number": 3,
            "type": "scip.SymbolInformation",
            "description": 'Symbols that are "defined" within this document.\n'
            "\n"
            " This should include symbols which technically do not have any definition,\n"
            " but have a reference and are defined by some other symbol (see\n"
            " Relationship.is_definition).",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    text: str = proto_field(
        default=...,
        spec={
            "name": "text",
            "number": 5,
            "type": "string",
            "description": "(optional) Text contents of this document. Indexers are not expected to\n"
            " include the text by default. It's preferable that clients read the text\n"
            " contents from the file system by resolving the absolute path from joining\n"
            " `Index.metadata.project_root` and `Document.relative_path`. This field\n"
            " can be useful for testing or when working with virtual/in-memory documents.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    position_encoding: PositionEncoding = proto_field(
        default=...,
        spec={
            "name": "position_encoding",
            "number": 6,
            "type": "scip.PositionEncoding",
            "description": "Specifies the encoding used for source ranges in this Document.\n"
            "\n"
            " Usually, this will match the type used to index the string type\n"
            " in the indexer's implementation language in O(1) time.\n"
            " - For an indexer implemented in JVM/.NET language or JavaScript/TypeScript,\n"
            "   use UTF16CodeUnitOffsetFromLineStart.\n"
            " - For an indexer implemented in Python,\n"
            "   use UTF32CodeUnitOffsetFromLineStart.\n"
            " - For an indexer implemented in Go, Rust or C++,\n"
            "   use UTF8ByteOffsetFromLineStart.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "scip",
        "name": "Symbol",
        "parent": None,
        "description": "Symbol is similar to a URI, it identifies a class, method, or a local\n"
        " variable. `SymbolInformation` contains rich metadata about symbols such as\n"
        " the docstring.\n"
        "\n"
        " Symbol has a standardized string representation, which can be used\n"
        " interchangeably with `Symbol`. The syntax for Symbol is the following:\n"
        " ```\n"
        " # (<x>)+ stands for one or more repetitions of <x>\n"
        " # (<x>)? stands for zero or one occurrence of <x>\n"
        " <symbol>               ::= <scheme> ' ' <package> ' ' (<descriptor>)+ | 'local ' <local-id>\n"
        " <package>              ::= <manager> ' ' <package-name> ' ' <version>\n"
        " <scheme>               ::= any UTF-8, escape spaces with double space. Must not be empty nor start "
        "with 'local'\n"
        " <manager>              ::= any UTF-8, escape spaces with double space. Use the placeholder '.' to "
        "indicate an empty value\n"
        " <package-name>         ::= same as above\n"
        " <version>              ::= same as above\n"
        " <descriptor>           ::= <namespace> | <type> | <term> | <method> | <type-parameter> | <parameter> "
        "| <meta> | <macro>\n"
        " <namespace>            ::= <name> '/'\n"
        " <type>                 ::= <name> '#'\n"
        " <term>                 ::= <name> '.'\n"
        " <meta>                 ::= <name> ':'\n"
        " <macro>                ::= <name> '!'\n"
        " <method>               ::= <name> '(' (<method-disambiguator>)? ').'\n"
        " <type-parameter>       ::= '[' <name> ']'\n"
        " <parameter>            ::= '(' <name> ')'\n"
        " <name>                 ::= <identifier>\n"
        " <method-disambiguator> ::= <simple-identifier>\n"
        " <identifier>           ::= <simple-identifier> | <escaped-identifier>\n"
        " <simple-identifier>    ::= (<identifier-character>)+\n"
        " <identifier-character> ::= '_' | '+' | '-' | '$' | ASCII letter or digit\n"
        " <escaped-identifier>   ::= '`' (<escaped-character>)+ '`', must contain at least one "
        "non-<identifier-character>\n"
        " <escaped-characters>   ::= any UTF-8, escape backticks with double backtick.\n"
        " <local-id>             ::= <simple-identifier>\n"
        " ```\n"
        "\n"
        " The list of descriptors for a symbol should together form a fully\n"
        " qualified name for the symbol. That is, it should serve as a unique\n"
        " identifier across the package. Typically, it will include one descriptor\n"
        " for every node in the AST (along the ancestry path) between the root of\n"
        " the file and the node corresponding to the symbol.\n"
        "\n"
        " Local symbols MUST only be used for entities which are local to a Document,\n"
        " and cannot be accessed from outside the Document.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Symbol(ProtoModel):
    """Symbol is similar to a URI, it identifies a class, method, or a local
    variable. `SymbolInformation` contains rich metadata about symbols such as
    the docstring.

    Symbol has a standardized string representation, which can be used
    interchangeably with `Symbol`. The syntax for Symbol is the following:
    ```
    # (<x>)+ stands for one or more repetitions of <x>
    # (<x>)? stands for zero or one occurrence of <x>
    <symbol>               ::= <scheme> ' ' <package> ' ' (<descriptor>)+ | 'local ' <local-id>
    <package>              ::= <manager> ' ' <package-name> ' ' <version>
    <scheme>               ::= any UTF-8, escape spaces with double space. Must not be empty nor start with 'local'
    <manager>              ::= any UTF-8, escape spaces with double space. Use the placeholder '.' to indicate an empty value
    <package-name>         ::= same as above
    <version>              ::= same as above
    <descriptor>           ::= <namespace> | <type> | <term> | <method> | <type-parameter> | <parameter> | <meta> | <macro>
    <namespace>            ::= <name> '/'
    <type>                 ::= <name> '#'
    <term>                 ::= <name> '.'
    <meta>                 ::= <name> ':'
    <macro>                ::= <name> '!'
    <method>               ::= <name> '(' (<method-disambiguator>)? ').'
    <type-parameter>       ::= '[' <name> ']'
    <parameter>            ::= '(' <name> ')'
    <name>                 ::= <identifier>
    <method-disambiguator> ::= <simple-identifier>
    <identifier>           ::= <simple-identifier> | <escaped-identifier>
    <simple-identifier>    ::= (<identifier-character>)+
    <identifier-character> ::= '_' | '+' | '-' | '$' | ASCII letter or digit
    <escaped-identifier>   ::= '`' (<escaped-character>)+ '`', must contain at least one non-<identifier-character>
    <escaped-characters>   ::= any UTF-8, escape backticks with double backtick.
    <local-id>             ::= <simple-identifier>
    ```

    The list of descriptors for a symbol should together form a fully
    qualified name for the symbol. That is, it should serve as a unique
    identifier across the package. Typically, it will include one descriptor
    for every node in the AST (along the ancestry path) between the root of
    the file and the node corresponding to the symbol.

    Local symbols MUST only be used for entities which are local to a Document,
    and cannot be accessed from outside the Document."""

    scheme: str = proto_field(
        default=...,
        spec={
            "name": "scheme",
            "number": 1,
            "type": "string",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    package: Package = proto_field(
        default=...,
        spec={
            "name": "package",
            "number": 2,
            "type": "scip.Package",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    descriptors: list[Descriptor] = proto_field(
        default=...,
        spec={
            "name": "descriptors",
            "number": 3,
            "type": "scip.Descriptor",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "scip",
        "name": "Package",
        "parent": None,
        "description": "Unit of packaging and distribution.\n\n NOTE: This corresponds to a module in Go and JVM languages.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Package(ProtoModel):
    """Unit of packaging and distribution.

    NOTE: This corresponds to a module in Go and JVM languages."""

    manager: str = proto_field(
        default=...,
        spec={
            "name": "manager",
            "number": 1,
            "type": "string",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    name: str = proto_field(
        default=...,
        spec={
            "name": "name",
            "number": 2,
            "type": "string",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    version: str = proto_field(
        default=...,
        spec={
            "name": "version",
            "number": 3,
            "type": "string",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_enum(
    {
        "package": "scip",
        "name": "Suffix",
        "parent": "scip.Descriptor",
        "description": None,
        "allow_alias": True,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {"name": "UnspecifiedSuffix", "number": 0, "deprecated": False},
            {
                "name": "Namespace",
                "number": 1,
                "description": "Unit of code abstraction and/or namespacing.\n"
                "\n"
                " NOTE: This corresponds to a package in Go and JVM languages.",
                "deprecated": False,
            },
            {
                "name": "Package",
                "number": 1,
                "description": "Use Namespace instead.",
                "deprecated": True,
            },
            {"name": "Type", "number": 2, "deprecated": False},
            {"name": "Term", "number": 3, "deprecated": False},
            {"name": "Method", "number": 4, "deprecated": False},
            {"name": "TypeParameter", "number": 5, "deprecated": False},
            {"name": "Parameter", "number": 6, "deprecated": False},
            {
                "name": "Meta",
                "number": 7,
                "description": "Can be used for any purpose.",
                "deprecated": False,
            },
            {"name": "Local", "number": 8, "deprecated": False},
            {"name": "Macro", "number": 9, "deprecated": False},
        ],
    }
)
class DescriptorSuffix(IntEnum):
    UnspecifiedSuffix = 0
    Namespace = 1
    Package = 1
    Type = 2
    Term = 3
    Method = 4
    TypeParameter = 5
    Parameter = 6
    Meta = 7
    Local = 8
    Macro = 9


@proto_message(
    {
        "package": "scip",
        "name": "Descriptor",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Descriptor(ProtoModel):
    name: str = proto_field(
        default=...,
        spec={
            "name": "name",
            "number": 1,
            "type": "string",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    disambiguator: str = proto_field(
        default=...,
        spec={
            "name": "disambiguator",
            "number": 2,
            "type": "string",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    suffix: DescriptorSuffix = proto_field(
        default=...,
        spec={
            "name": "suffix",
            "number": 3,
            "type": "scip.Descriptor.Suffix",
            "description": "NOTE: If you add new fields here, make sure to update the prepareSlot()\n"
            " function responsible for parsing symbols.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "scip",
        "name": "Signature",
        "parent": None,
        "description": "Signature represents the signature of a symbol as it's displayed in API\n"
        " documentation or hover tooltips. It uses a subset of Document's fields with\n"
        " the same field numbers for wire compatibility with older indexes that encoded\n"
        " signatures using the Document message type.",
        "oneofs": [],
        "reserved_numbers": [1, 3, 6],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Signature(ProtoModel):
    """Signature represents the signature of a symbol as it's displayed in API
    documentation or hover tooltips. It uses a subset of Document's fields with
    the same field numbers for wire compatibility with older indexes that encoded
    signatures using the Document message type."""

    language: str = proto_field(
        default=...,
        spec={
            "name": "language",
            "number": 4,
            "type": "string",
            "description": 'The language of the signature, e.g. "java", "go", "python".',
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    text: str = proto_field(
        default=...,
        spec={
            "name": "text",
            "number": 5,
            "type": "string",
            "description": 'The text content of the signature, e.g. "void add(int a, int b)".',
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    occurrences: list[Occurrence] = proto_field(
        default=...,
        spec={
            "name": "occurrences",
            "number": 2,
            "type": "scip.Occurrence",
            "description": "(optional) Occurrences within the signature text that reference other\n"
            " symbols, enabling hyperlinking of types in the signature. Ranges are\n"
            " relative to the `text` field.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_enum(
    {
        "package": "scip",
        "name": "Kind",
        "parent": "scip.SymbolInformation",
        "description": "(optional) Kind represents the fine-grained category of a symbol, suitable for presenting\n"
        " information about the symbol's meaning in the language.\n"
        "\n"
        " For example:\n"
        " - A Java method would have the kind `Method` while a Go function would\n"
        "   have the kind `Function`, even if the symbols for these use the same\n"
        "   syntax for the descriptor `SymbolDescriptor.Suffix.Method`.\n"
        " - A Go struct has the symbol kind `Struct` while a Java class has\n"
        "   the symbol kind `Class` even if they both have the same descriptor:\n"
        "   `SymbolDescriptor.Suffix.Type`.\n"
        "\n"
        " Since Kind is more fine-grained than Suffix:\n"
        " - If two symbols have the same Kind, they should share the same Suffix.\n"
        " - If two symbols have different Suffixes, they should have different Kinds.",
        "allow_alias": False,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {"name": "UnspecifiedKind", "number": 0, "deprecated": False},
            {
                "name": "AbstractMethod",
                "number": 66,
                "description": "A method which may or may not have a body. For Java, Kotlin etc.",
                "deprecated": False,
            },
            {
                "name": "Accessor",
                "number": 72,
                "description": "For Ruby's attr_accessor",
                "deprecated": False,
            },
            {"name": "Array", "number": 1, "deprecated": False},
            {
                "name": "Assertion",
                "number": 2,
                "description": "For Alloy",
                "deprecated": False,
            },
            {"name": "AssociatedType", "number": 3, "deprecated": False},
            {
                "name": "Attribute",
                "number": 4,
                "description": "For C++",
                "deprecated": False,
            },
            {
                "name": "Axiom",
                "number": 5,
                "description": "For Lean",
                "deprecated": False,
            },
            {"name": "Boolean", "number": 6, "deprecated": False},
            {"name": "Class", "number": 7, "deprecated": False},
            {
                "name": "Concept",
                "number": 86,
                "description": "For C++",
                "deprecated": False,
            },
            {"name": "Constant", "number": 8, "deprecated": False},
            {"name": "Constructor", "number": 9, "deprecated": False},
            {
                "name": "Contract",
                "number": 62,
                "description": "For Solidity",
                "deprecated": False,
            },
            {
                "name": "DataFamily",
                "number": 10,
                "description": "For Haskell",
                "deprecated": False,
            },
            {
                "name": "Delegate",
                "number": 73,
                "description": "For C# and F#",
                "deprecated": False,
            },
            {"name": "Enum", "number": 11, "deprecated": False},
            {"name": "EnumMember", "number": 12, "deprecated": False},
            {"name": "Error", "number": 63, "deprecated": False},
            {"name": "Event", "number": 13, "deprecated": False},
            {
                "name": "Extension",
                "number": 84,
                "description": "For Dart",
                "deprecated": False,
            },
            {
                "name": "Fact",
                "number": 14,
                "description": "For Alloy",
                "deprecated": False,
            },
            {"name": "Field", "number": 15, "deprecated": False},
            {"name": "File", "number": 16, "deprecated": False},
            {"name": "Function", "number": 17, "deprecated": False},
            {
                "name": "Getter",
                "number": 18,
                "description": "For 'get' in Swift, 'attr_reader' in Ruby",
                "deprecated": False,
            },
            {
                "name": "Grammar",
                "number": 19,
                "description": "For Raku",
                "deprecated": False,
            },
            {
                "name": "Instance",
                "number": 20,
                "description": "For Purescript and Lean",
                "deprecated": False,
            },
            {"name": "Interface", "number": 21, "deprecated": False},
            {"name": "Key", "number": 22, "deprecated": False},
            {
                "name": "Lang",
                "number": 23,
                "description": "For Racket",
                "deprecated": False,
            },
            {
                "name": "Lemma",
                "number": 24,
                "description": "For Lean",
                "deprecated": False,
            },
            {
                "name": "Library",
                "number": 64,
                "description": "For solidity",
                "deprecated": False,
            },
            {"name": "Macro", "number": 25, "deprecated": False},
            {"name": "Method", "number": 26, "deprecated": False},
            {
                "name": "MethodAlias",
                "number": 74,
                "description": "For Ruby",
                "deprecated": False,
            },
            {
                "name": "MethodReceiver",
                "number": 27,
                "description": "Analogous to 'ThisParameter' and 'SelfParameter', but for languages\n"
                " like Go where the receiver doesn't have a conventional name.",
                "deprecated": False,
            },
            {
                "name": "MethodSpecification",
                "number": 67,
                "description": "Analogous to 'AbstractMethod', for Go.",
                "deprecated": False,
            },
            {
                "name": "Message",
                "number": 28,
                "description": "For Protobuf",
                "deprecated": False,
            },
            {
                "name": "Mixin",
                "number": 85,
                "description": "For Dart",
                "deprecated": False,
            },
            {
                "name": "Modifier",
                "number": 65,
                "description": "For Solidity",
                "deprecated": False,
            },
            {"name": "Module", "number": 29, "deprecated": False},
            {"name": "Namespace", "number": 30, "deprecated": False},
            {"name": "Null", "number": 31, "deprecated": False},
            {"name": "Number", "number": 32, "deprecated": False},
            {"name": "Object", "number": 33, "deprecated": False},
            {"name": "Operator", "number": 34, "deprecated": False},
            {"name": "Package", "number": 35, "deprecated": False},
            {"name": "PackageObject", "number": 36, "deprecated": False},
            {"name": "Parameter", "number": 37, "deprecated": False},
            {"name": "ParameterLabel", "number": 38, "deprecated": False},
            {
                "name": "Pattern",
                "number": 39,
                "description": "For Haskell's PatternSynonyms",
                "deprecated": False,
            },
            {
                "name": "Predicate",
                "number": 40,
                "description": "For Alloy",
                "deprecated": False,
            },
            {"name": "Property", "number": 41, "deprecated": False},
            {
                "name": "Protocol",
                "number": 42,
                "description": "Analogous to 'Trait' and 'TypeClass', for Swift and Objective-C",
                "deprecated": False,
            },
            {
                "name": "ProtocolMethod",
                "number": 68,
                "description": "Analogous to 'AbstractMethod', for Swift and Objective-C.",
                "deprecated": False,
            },
            {
                "name": "PureVirtualMethod",
                "number": 69,
                "description": "Analogous to 'AbstractMethod', for C++.",
                "deprecated": False,
            },
            {
                "name": "Quasiquoter",
                "number": 43,
                "description": "For Haskell",
                "deprecated": False,
            },
            {
                "name": "SelfParameter",
                "number": 44,
                "description": "'self' in Python, Rust, Swift etc.",
                "deprecated": False,
            },
            {
                "name": "Setter",
                "number": 45,
                "description": "For 'set' in Swift, 'attr_writer' in Ruby",
                "deprecated": False,
            },
            {
                "name": "Signature",
                "number": 46,
                "description": "For Alloy, analogous to 'Struct'.",
                "deprecated": False,
            },
            {
                "name": "SingletonClass",
                "number": 75,
                "description": "For Ruby",
                "deprecated": False,
            },
            {
                "name": "SingletonMethod",
                "number": 76,
                "description": "Analogous to 'StaticMethod', for Ruby.",
                "deprecated": False,
            },
            {
                "name": "StaticDataMember",
                "number": 77,
                "description": "Analogous to 'StaticField', for C++",
                "deprecated": False,
            },
            {
                "name": "StaticEvent",
                "number": 78,
                "description": "For C#",
                "deprecated": False,
            },
            {
                "name": "StaticField",
                "number": 79,
                "description": "For C#",
                "deprecated": False,
            },
            {
                "name": "StaticMethod",
                "number": 80,
                "description": "For Java, C#, C++ etc.",
                "deprecated": False,
            },
            {
                "name": "StaticProperty",
                "number": 81,
                "description": "For C#, TypeScript etc.",
                "deprecated": False,
            },
            {
                "name": "StaticVariable",
                "number": 82,
                "description": "For C, C++",
                "deprecated": False,
            },
            {"name": "String", "number": 48, "deprecated": False},
            {"name": "Struct", "number": 49, "deprecated": False},
            {
                "name": "Subscript",
                "number": 47,
                "description": "For Swift",
                "deprecated": False,
            },
            {
                "name": "Tactic",
                "number": 50,
                "description": "For Lean",
                "deprecated": False,
            },
            {
                "name": "Theorem",
                "number": 51,
                "description": "For Lean",
                "deprecated": False,
            },
            {
                "name": "ThisParameter",
                "number": 52,
                "description": "Method receiver for languages\n 'this' in JavaScript, C++, Java etc.",
                "deprecated": False,
            },
            {
                "name": "Trait",
                "number": 53,
                "description": "Analogous to 'Protocol' and 'TypeClass', for Rust, Scala etc.",
                "deprecated": False,
            },
            {
                "name": "TraitMethod",
                "number": 70,
                "description": "Analogous to 'AbstractMethod', for Rust, Scala etc.",
                "deprecated": False,
            },
            {
                "name": "Type",
                "number": 54,
                "description": "Data type definition for languages like OCaml which use `type`\n"
                " rather than separate keywords like `struct` and `enum`.",
                "deprecated": False,
            },
            {"name": "TypeAlias", "number": 55, "deprecated": False},
            {
                "name": "TypeClass",
                "number": 56,
                "description": "Analogous to 'Trait' and 'Protocol', for Haskell, Purescript etc.",
                "deprecated": False,
            },
            {
                "name": "TypeClassMethod",
                "number": 71,
                "description": "Analogous to 'AbstractMethod', for Haskell, Purescript etc.",
                "deprecated": False,
            },
            {
                "name": "TypeFamily",
                "number": 57,
                "description": "For Haskell",
                "deprecated": False,
            },
            {"name": "TypeParameter", "number": 58, "deprecated": False},
            {
                "name": "Union",
                "number": 59,
                "description": "For C, C++, Capn Proto",
                "deprecated": False,
            },
            {"name": "Value", "number": 60, "deprecated": False},
            {
                "name": "Variable",
                "number": 61,
                "description": "Next = 87;\n Feel free to open a PR proposing new language-specific kinds.",
                "deprecated": False,
            },
        ],
    }
)
class SymbolInformationKind(IntEnum):
    """(optional) Kind represents the fine-grained category of a symbol, suitable for presenting
    information about the symbol's meaning in the language.

    For example:
    - A Java method would have the kind `Method` while a Go function would
      have the kind `Function`, even if the symbols for these use the same
      syntax for the descriptor `SymbolDescriptor.Suffix.Method`.
    - A Go struct has the symbol kind `Struct` while a Java class has
      the symbol kind `Class` even if they both have the same descriptor:
      `SymbolDescriptor.Suffix.Type`.

    Since Kind is more fine-grained than Suffix:
    - If two symbols have the same Kind, they should share the same Suffix.
    - If two symbols have different Suffixes, they should have different Kinds."""

    UnspecifiedKind = 0
    AbstractMethod = 66
    Accessor = 72
    Array = 1
    Assertion = 2
    AssociatedType = 3
    Attribute = 4
    Axiom = 5
    Boolean = 6
    Class = 7
    Concept = 86
    Constant = 8
    Constructor = 9
    Contract = 62
    DataFamily = 10
    Delegate = 73
    Enum = 11
    EnumMember = 12
    Error = 63
    Event = 13
    Extension = 84
    Fact = 14
    Field = 15
    File = 16
    Function = 17
    Getter = 18
    Grammar = 19
    Instance = 20
    Interface = 21
    Key = 22
    Lang = 23
    Lemma = 24
    Library = 64
    Macro = 25
    Method = 26
    MethodAlias = 74
    MethodReceiver = 27
    MethodSpecification = 67
    Message = 28
    Mixin = 85
    Modifier = 65
    Module = 29
    Namespace = 30
    Null = 31
    Number = 32
    Object = 33
    Operator = 34
    Package = 35
    PackageObject = 36
    Parameter = 37
    ParameterLabel = 38
    Pattern = 39
    Predicate = 40
    Property = 41
    Protocol = 42
    ProtocolMethod = 68
    PureVirtualMethod = 69
    Quasiquoter = 43
    SelfParameter = 44
    Setter = 45
    Signature = 46
    SingletonClass = 75
    SingletonMethod = 76
    StaticDataMember = 77
    StaticEvent = 78
    StaticField = 79
    StaticMethod = 80
    StaticProperty = 81
    StaticVariable = 82
    String = 48
    Struct = 49
    Subscript = 47
    Tactic = 50
    Theorem = 51
    ThisParameter = 52
    Trait = 53
    TraitMethod = 70
    Type = 54
    TypeAlias = 55
    TypeClass = 56
    TypeClassMethod = 71
    TypeFamily = 57
    TypeParameter = 58
    Union = 59
    Value = 60
    Variable = 61


@proto_message(
    {
        "package": "scip",
        "name": "SymbolInformation",
        "parent": None,
        "description": "SymbolInformation defines metadata about a symbol, such as the symbol's\n"
        " docstring or what package it's defined it.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class SymbolInformation(ProtoModel):
    """SymbolInformation defines metadata about a symbol, such as the symbol's
    docstring or what package it's defined it."""

    symbol: str = proto_field(
        default=...,
        spec={
            "name": "symbol",
            "number": 1,
            "type": "string",
            "description": "Identifier of this symbol, which can be referenced from `Occurence.symbol`.\n"
            " The string must be formatted according to the grammar in `Symbol`.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    documentation: list[str] = proto_field(
        default=...,
        spec={
            "name": "documentation",
            "number": 3,
            "type": "string",
            "description": "(optional, but strongly recommended) The markdown-formatted documentation\n"
            " for this symbol. Use `SymbolInformation.signature_documentation` to\n"
            " document the method/class/type signature of this symbol.\n"
            " Due to historical reasons, indexers may include signature documentation in\n"
            " this field by rendering markdown code blocks. New indexers should only\n"
            " include non-code documentation in this field, for example docstrings.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    relationships: list[Relationship] = proto_field(
        default=...,
        spec={
            "name": "relationships",
            "number": 4,
            "type": "scip.Relationship",
            "description": "(optional) Relationships to other symbols (e.g., implements, type definition).",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    kind: SymbolInformationKind = proto_field(
        default=...,
        spec={
            "name": "kind",
            "number": 5,
            "type": "scip.SymbolInformation.Kind",
            "description": "The kind of this symbol. Use this field instead of\n"
            " `SymbolDescriptor.Suffix` to determine whether something is, for example, a\n"
            " class or a method.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    display_name: str = proto_field(
        default=...,
        spec={
            "name": "display_name",
            "number": 6,
            "type": "string",
            "description": "(optional) The name of this symbol as it should be displayed to the user.\n"
            ' For example, the symbol "com/example/MyClass#myMethod(+1)." should have the\n'
            ' display name "myMethod". The `symbol` field is not a reliable source of\n'
            " the display name for several reasons:\n"
            "\n"
            " - Local symbols don't encode the name.\n"
            " - Some languages have case-insensitive names, so the symbol is all-lowercase.\n"
            " - The symbol may encode names with special characters that should not be\n"
            "   displayed to the user.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    signature_documentation: Signature = proto_field(
        default=...,
        spec={
            "name": "signature_documentation",
            "number": 7,
            "type": "scip.Signature",
            "description": "(optional) The signature of this symbol as it's displayed in API\n"
            " documentation or in hover tooltips. For example, a Java method that adds\n"
            ' two numbers would have `Signature.language = "java"` and\n'
            ' `Signature.text = "void add(int a, int b)"`. The `language` and `text`\n'
            " fields are required while `occurrences` can be optionally included to\n"
            " support hyperlinking referenced symbols in the signature.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    enclosing_symbol: str = proto_field(
        default=...,
        spec={
            "name": "enclosing_symbol",
            "number": 8,
            "type": "string",
            "description": "(optional) The enclosing symbol if this is a local symbol.  For non-local\n"
            " symbols, the enclosing symbol should be parsed from the `symbol` field\n"
            " using the `Descriptor` grammar.\n"
            "\n"
            " The primary use-case for this field is to allow local symbol to be displayed\n"
            " in a symbol hierarchy for API documentation. It's OK to leave this field\n"
            " empty for local variables since local variables usually don't belong in API\n"
            " documentation. However, in the situation that you wish to include a local\n"
            " symbol in the hierarchy, then you can use `enclosing_symbol` to locate the\n"
            ' "parent" or "owner" of this local symbol. For example, a Java indexer may\n'
            " choose to use local symbols for private class fields while providing an\n"
            " `enclosing_symbol` to reference the enclosing class to allow the field to\n"
            " be part of the class documentation hierarchy. From the perspective of an\n"
            " author of an indexer, the decision to use a local symbol or global symbol\n"
            " should exclusively be determined whether the local symbol is accessible\n"
            " outside the document, not by the capability to find the enclosing\n"
            " symbol.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "scip",
        "name": "Relationship",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Relationship(ProtoModel):
    symbol: str = proto_field(
        default=...,
        spec={
            "name": "symbol",
            "number": 1,
            "type": "string",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    is_reference: bool = proto_field(
        default=...,
        spec={
            "name": "is_reference",
            "number": 2,
            "type": "bool",
            "description": 'When resolving "Find references", this field documents what other symbols\n'
            " should be included together with this symbol. For example, consider the\n"
            " following TypeScript code that defines two symbols `Animal#sound()` and\n"
            " `Dog#sound()`:\n"
            " ```ts\n"
            " interface Animal {\n"
            "           ^^^^^^ definition Animal#\n"
            "   sound(): string\n"
            "   ^^^^^ definition Animal#sound()\n"
            " }\n"
            " class Dog implements Animal {\n"
            '       ^^^ definition Dog#, relationships = [{symbol: "Animal#", is_implementation: true}]\n'
            '   public sound(): string { return "woof" }\n'
            "          ^^^^^ definition Dog#sound(), references_symbols = Animal#sound(), relationships = "
            '[{symbol: "Animal#sound()", is_implementation:true, is_reference: true}]\n'
            " }\n"
            " const animal: Animal = new Dog()\n"
            "               ^^^^^^ reference Animal#\n"
            " console.log(animal.sound())\n"
            "                    ^^^^^ reference Animal#sound()\n"
            " ```\n"
            ' Doing "Find references" on the symbol `Animal#sound()` should return\n'
            ' references to the `Dog#sound()` method as well. Vice-versa, doing "Find\n'
            ' references" on the `Dog#sound()` method should include references to the\n'
            " `Animal#sound()` method as well.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    is_implementation: bool = proto_field(
        default=...,
        spec={
            "name": "is_implementation",
            "number": 3,
            "type": "bool",
            "description": 'Similar to `is_reference` but for "Find implementations".\n'
            " It's common for `is_implementation` and `is_reference` to both be true but\n"
            " it's not always the case.\n"
            " In the TypeScript example above, observe that `Dog#` has an\n"
            ' `is_implementation` relationship with `"Animal#"` but not `is_reference`.\n'
            ' This is because "Find references" on the "Animal#" symbol should not return\n'
            ' "Dog#". We only want "Dog#" to return as a result for "Find\n'
            ' implementations" on the "Animal#" symbol.',
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    is_type_definition: bool = proto_field(
        default=...,
        spec={
            "name": "is_type_definition",
            "number": 4,
            "type": "bool",
            "description": 'Similar to `references_symbols` but for "Go to type definition".',
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    is_definition: bool = proto_field(
        default=...,
        spec={
            "name": "is_definition",
            "number": 5,
            "type": "bool",
            "description": 'Allows overriding the behavior of "Go to definition" and "Find references"\n'
            " for symbols which do not have a definition of their own or could\n"
            " potentially have multiple definitions.\n"
            "\n"
            " For example, in a language with single inheritance and no field overriding,\n"
            " inherited fields can reuse the same symbol as the ancestor which declares\n"
            " the field. In such a situation, is_definition is not needed.\n"
            "\n"
            " On the other hand, in languages with single inheritance and some form\n"
            " of mixins, you can use is_definition to relate the symbol to the\n"
            " matching symbol in ancestor classes, and is_reference to relate the\n"
            " symbol to the matching symbol in mixins.\n"
            "\n"
            "Update registerInverseRelationships on adding a new field here.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "scip",
        "name": "SingleLineRange",
        "parent": None,
        "description": "SingleLineRange represents a half-open [start, end) range within a single line.\n"
        "\n"
        " Line numbers and characters are always 0-based. Make sure to increment them\n"
        " before displaying in an editor-like UI because editors conventionally use\n"
        " 1-based numbers. The `character` values are interpreted based on the\n"
        " `PositionEncoding` for the enclosing Document.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class SingleLineRange(ProtoModel):
    """SingleLineRange represents a half-open [start, end) range within a single line.

    Line numbers and characters are always 0-based. Make sure to increment them
    before displaying in an editor-like UI because editors conventionally use
    1-based numbers. The `character` values are interpreted based on the
    `PositionEncoding` for the enclosing Document."""

    line: int = proto_field(
        default=...,
        spec={
            "name": "line",
            "number": 1,
            "type": "int32",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    start_character: int = proto_field(
        default=...,
        spec={
            "name": "start_character",
            "number": 2,
            "type": "int32",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    end_character: int = proto_field(
        default=...,
        spec={
            "name": "end_character",
            "number": 3,
            "type": "int32",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "scip",
        "name": "MultiLineRange",
        "parent": None,
        "description": "MultiLineRange represents a half-open [start, end) range spanning multiple lines.\n"
        "\n"
        " Line numbers and characters are always 0-based. Make sure to increment them\n"
        " before displaying in an editor-like UI because editors conventionally use\n"
        " 1-based numbers. The `character` values are interpreted based on the\n"
        " `PositionEncoding` for the enclosing Document.\n"
        "\n"
        " Producers SHOULD use `SingleLineRange` when `start_line == end_line` to keep\n"
        " indexes compact, but consumers MUST accept multi-line encoding even when the\n"
        " range happens to fit on a single line.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class MultiLineRange(ProtoModel):
    """MultiLineRange represents a half-open [start, end) range spanning multiple lines.

    Line numbers and characters are always 0-based. Make sure to increment them
    before displaying in an editor-like UI because editors conventionally use
    1-based numbers. The `character` values are interpreted based on the
    `PositionEncoding` for the enclosing Document.

    Producers SHOULD use `SingleLineRange` when `start_line == end_line` to keep
    indexes compact, but consumers MUST accept multi-line encoding even when the
    range happens to fit on a single line."""

    start_line: int = proto_field(
        default=...,
        spec={
            "name": "start_line",
            "number": 1,
            "type": "int32",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    start_character: int = proto_field(
        default=...,
        spec={
            "name": "start_character",
            "number": 2,
            "type": "int32",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    end_line: int = proto_field(
        default=...,
        spec={
            "name": "end_line",
            "number": 3,
            "type": "int32",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    end_character: int = proto_field(
        default=...,
        spec={
            "name": "end_character",
            "number": 4,
            "type": "int32",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "scip",
        "name": "Occurrence",
        "parent": None,
        "description": "Occurrence associates a source position with a symbol and/or highlighting\n"
        " information.\n"
        "\n"
        " If possible, indexers should try to bundle logically related information\n"
        " across occurrences into a single occurrence to reduce payload sizes.\n"
        "\n"
        " Range encoding:\n"
        "\n"
        " An Occurrence carries its source range in one of two ways: the deprecated\n"
        " `range` field (a `repeated int32` packed encoding kept for backward\n"
        " compatibility), or one of the typed alternatives in the `typed_range`\n"
        " oneof. New producers SHOULD set `typed_range` and SHOULD NOT set the\n"
        " deprecated `range` field. The same rule applies to `enclosing_range` and\n"
        " `typed_enclosing_range`.\n"
        "\n"
        " When both encodings are present on the same Occurrence, `typed_range` takes\n"
        " precedence over `range` (likewise `typed_enclosing_range` over\n"
        " `enclosing_range`). Producers that set both forms MUST keep them\n"
        " semantically equivalent. Consumers SHOULD prefer the typed form when\n"
        " available and fall back to the `repeated int32` form otherwise.",
        "oneofs": ["typed_range", "typed_enclosing_range"],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Occurrence(ProtoModel):
    """Occurrence associates a source position with a symbol and/or highlighting
    information.

    If possible, indexers should try to bundle logically related information
    across occurrences into a single occurrence to reduce payload sizes.

    Range encoding:

    An Occurrence carries its source range in one of two ways: the deprecated
    `range` field (a `repeated int32` packed encoding kept for backward
    compatibility), or one of the typed alternatives in the `typed_range`
    oneof. New producers SHOULD set `typed_range` and SHOULD NOT set the
    deprecated `range` field. The same rule applies to `enclosing_range` and
    `typed_enclosing_range`.

    When both encodings are present on the same Occurrence, `typed_range` takes
    precedence over `range` (likewise `typed_enclosing_range` over
    `enclosing_range`). Producers that set both forms MUST keep them
    semantically equivalent. Consumers SHOULD prefer the typed form when
    available and fall back to the `repeated int32` form otherwise."""

    range: list[int] = proto_field(
        default=...,
        spec={
            "name": "range",
            "number": 1,
            "type": "int32",
            "description": "Deprecated: Use `single_line_range` or `multi_line_range` instead.\n"
            "\n"
            " Half-open [start, end) range. Must be exactly three or four elements:\n"
            " - Three elements: `[startLine, startCharacter, endCharacter]` (single-line)\n"
            " - Four elements: `[startLine, startCharacter, endLine, endCharacter]`\n"
            "\n"
            " The end line of a three-element range is inferred to equal the start line.\n"
            "\n"
            " Historical note: the original draft of this schema had a `Range` message\n"
            " type with `start` and `end` fields of type `Position`, mirroring LSP.\n"
            " Benchmarks revealed that this encoding was inefficient and that we could\n"
            " reduce the total payload size of an index by 50% by using `repeated int32`\n"
            " instead. However, the lack of type safety led to the introduction of\n"
            " `single_line_range` and `multi_line_range` as typed alternatives; the\n"
            " typed encoding's per-index size overhead is small (single-digit percent)\n"
            " because ranges are only a fraction of a typical index payload.",
            "repeated": True,
            "optional": False,
            "deprecated": True,
        },
    )
    single_line_range: SingleLineRange | None = proto_field(
        default=None,
        spec={
            "name": "single_line_range",
            "number": 8,
            "type": "scip.SingleLineRange",
            "description": "Range spanning a single line.",
            "repeated": False,
            "optional": False,
            "oneof": "typed_range",
            "deprecated": False,
        },
    )
    multi_line_range: MultiLineRange | None = proto_field(
        default=None,
        spec={
            "name": "multi_line_range",
            "number": 9,
            "type": "scip.MultiLineRange",
            "description": "Range spanning multiple lines.",
            "repeated": False,
            "optional": False,
            "oneof": "typed_range",
            "deprecated": False,
        },
    )
    symbol: str = proto_field(
        default=...,
        spec={
            "name": "symbol",
            "number": 2,
            "type": "string",
            "description": "(optional) The symbol that appears at this position. See\n"
            " `SymbolInformation.symbol` for how to format symbols as strings.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    symbol_roles: int = proto_field(
        default=...,
        spec={
            "name": "symbol_roles",
            "number": 3,
            "type": "int32",
            "description": "(optional) Bitset containing `SymbolRole`s in this occurrence.\n"
            " See `SymbolRole`'s documentation for how to read and write this field.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    override_documentation: list[str] = proto_field(
        default=...,
        spec={
            "name": "override_documentation",
            "number": 4,
            "type": "string",
            "description": "(optional) CommonMark-formatted documentation for this specific range. If\n"
            " empty, the `Symbol.documentation` field is used instead. One example\n"
            " where this field might be useful is when the symbol represents a generic\n"
            " function (with abstract type parameters such as `List<T>`) and at this\n"
            " occurrence we know the exact values (such as `List<String>`).\n"
            "\n"
            " This field can also be used for dynamically or gradually typed languages,\n"
            " which commonly allow for type-changing assignment.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    syntax_kind: SyntaxKind = proto_field(
        default=...,
        spec={
            "name": "syntax_kind",
            "number": 5,
            "type": "scip.SyntaxKind",
            "description": "(optional) What syntax highlighting class should be used for this range?",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    diagnostics: list[Diagnostic] = proto_field(
        default=...,
        spec={
            "name": "diagnostics",
            "number": 6,
            "type": "scip.Diagnostic",
            "description": "(optional) Diagnostics that have been reported for this specific range.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    enclosing_range: list[int] = proto_field(
        default=...,
        spec={
            "name": "enclosing_range",
            "number": 7,
            "type": "int32",
            "description": "Deprecated: Use `typed_enclosing_range` instead.\n"
            "\n"
            " Uses the same `repeated int32` encoding as the deprecated `range` field.",
            "repeated": True,
            "optional": False,
            "deprecated": True,
        },
    )
    single_line_enclosing_range: SingleLineRange | None = proto_field(
        default=None,
        spec={
            "name": "single_line_enclosing_range",
            "number": 10,
            "type": "scip.SingleLineRange",
            "description": "Enclosing range spanning a single line.",
            "repeated": False,
            "optional": False,
            "oneof": "typed_enclosing_range",
            "deprecated": False,
        },
    )
    multi_line_enclosing_range: MultiLineRange | None = proto_field(
        default=None,
        spec={
            "name": "multi_line_enclosing_range",
            "number": 11,
            "type": "scip.MultiLineRange",
            "description": "Enclosing range spanning multiple lines.",
            "repeated": False,
            "optional": False,
            "oneof": "typed_enclosing_range",
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "scip",
        "name": "Diagnostic",
        "parent": None,
        "description": "Represents a diagnostic, such as a compiler error or warning, which should be\n"
        " reported for a document.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Diagnostic(ProtoModel):
    """Represents a diagnostic, such as a compiler error or warning, which should be
    reported for a document."""

    severity: Severity = proto_field(
        default=...,
        spec={
            "name": "severity",
            "number": 1,
            "type": "scip.Severity",
            "description": "Should this diagnostic be reported as an error, warning, info, or hint?",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    code: str = proto_field(
        default=...,
        spec={
            "name": "code",
            "number": 2,
            "type": "string",
            "description": "(optional) Code of this diagnostic, which might appear in the user interface.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    message: str = proto_field(
        default=...,
        spec={
            "name": "message",
            "number": 3,
            "type": "string",
            "description": "Message of this diagnostic.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    source: str = proto_field(
        default=...,
        spec={
            "name": "source",
            "number": 4,
            "type": "string",
            "description": "(optional) Human-readable string describing the source of this diagnostic, e.g.\n"
            " 'typescript' or 'super lint'.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    tags: list[DiagnosticTag] = proto_field(
        default=...,
        spec={
            "name": "tags",
            "number": 5,
            "type": "scip.DiagnosticTag",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


SCIP_PACKAGE = ProtoPackage(
    spec={
        "path": "scip/scip.proto",
        "package": "scip",
        "upstream_release": "v0.9.0",
        "upstream_schema": "https://github.com/scip-code/scip/blob/v0.9.0/scip.proto",
        "description": None,
        "imports": [],
        "options": {
            "go_package": "github.com/scip-code/scip/bindings/go/scip/",
            "java_multiple_files": True,
            "java_outer_classname": "ScipProto",
            "java_package": "org.scip_code.scip",
        },
        "section_option": False,
    },
    models=(
        Index,
        Metadata,
        ToolInfo,
        Document,
        Symbol,
        Package,
        Descriptor,
        Signature,
        SymbolInformation,
        Relationship,
        SingleLineRange,
        MultiLineRange,
        Occurrence,
        Diagnostic,
    ),
    enums=(
        ProtocolVersion,
        TextEncoding,
        PositionEncoding,
        SymbolRole,
        SyntaxKind,
        Severity,
        DiagnosticTag,
        Language,
    ),
    services=[],
)
