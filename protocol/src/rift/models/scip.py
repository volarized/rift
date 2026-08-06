from __future__ import annotations

from .base import *


@proto_enum(DirectEnum(SCIP))
class ProtocolVersion(IntEnum):
    UnspecifiedProtocolVersion = 0


@proto_enum(DirectEnum(SCIP))
class TextEncoding(IntEnum):
    UnspecifiedTextEncoding = 0
    UTF8 = 1
    UTF16 = 2


@proto_enum(
    DirectEnum(
        SCIP,
        description="Encoding used to interpret the 'character' value in source ranges.",
        value_descriptions=(
            (
                "Default value. This value should not be used by new SCIP indexers\n so that a "
                "consumer can process the SCIP index without ambiguity."
            ),
            (
                "The 'character' value is interpreted as an offset in terms\n of UTF-8 code units "
                '(i.e. bytes).\n\n Example: For the string "🚀 Woo" in UTF-8, the bytes are\n [240, '
                "159, 154, 128, 32, 87, 111, 111], so the offset for 'W'\n would be 5."
            ),
            (
                "The 'character' value is interpreted as an offset in terms\n of UTF-16 code units "
                '(each is 2 bytes).\n\n Example: For the string "🚀 Woo", the UTF-16 code units are\n '
                "['\\ud83d', '\\ude80', ' ', 'W', 'o', 'o'], so the offset for 'W'\n would be 3."
            ),
            (
                "The 'character' value is interpreted as an offset in terms\n of UTF-32 code units "
                '(each is 4 bytes).\n\n Example: For the string "🚀 Woo", the UTF-32 code units are\n '
                "['🚀', ' ', 'W', 'o', 'o'], so the offset for 'W' would be 2."
            ),
        ),
    )
)
class PositionEncoding(IntEnum):
    """Encoding used to interpret the 'character' value in source ranges."""

    UnspecifiedPositionEncoding = 0
    UTF8CodeUnitOffsetFromLineStart = 1
    UTF16CodeUnitOffsetFromLineStart = 2
    UTF32CodeUnitOffsetFromLineStart = 3


@proto_enum(
    DirectEnum(
        SCIP,
        description=(
            'SymbolRole declares what "role" a symbol has in an occurrence. A role is\n '
            "encoded as a bitset where each bit represents a different role. For example,\n to "
            "determine if the `Import` role is set, test whether the second bit of the\n enum "
            "value is defined. In pseudocode, this can be implemented with the\n logic: `const "
            "isImportRole = (role.value & SymbolRole.Import.value) > 0`."
        ),
        value_descriptions=(
            "This case is not meant to be used; it only exists to avoid an error\n from the Protobuf code generator.",
            "Is the symbol defined here? If not, then this is a symbol reference.",
            "Is the symbol imported here?",
            "Is the symbol written here?",
            "Is the symbol read here?",
            "Is the symbol in generated code?",
            "Is the symbol in test code?",
            (
                "Is this a signature for a symbol that is defined elsewhere?\n\n Applies to forward "
                "declarations for languages like C, C++\n and Objective-C, as well as `val` "
                "declarations in interface\n files in languages like SML and OCaml."
            ),
        ),
    )
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
    DirectEnum(
        SCIP,
        value_descriptions=(
            None,
            "Comment, including comment markers and text",
            "`;` `.` `,`",
            "(), {}, [] when used syntactically",
            "`if`, `else`, `return`, `class`, etc.",
            None,
            "`+`, `*`, etc.",
            "non-specific catch-all for any identifier not better described elsewhere",
            "Identifiers builtin to the language: `min`, `print` in Python.",
            "Identifiers representing `null`-like values: `None` in Python, `nil` in Go.",
            '`xyz` in `const xyz = "hello"`',
            '`var X = "hello"` in Go',
            "Parameter definition and references",
            "Identifiers for variable definitions and references within a local scope",
            "Identifiers that shadow other identifiers in an outer scope",
            (
                "Identifier representing a unit of code abstraction and/or namespacing.\n\n NOTE: "
                "This corresponds to a package in Go and JVM languages,\n and a module in "
                "languages like Python and JavaScript."
            ),
            None,
            "Function references, including calls",
            "Function definition only",
            "Macro references, including invocations",
            "Macro definition only",
            "non-builtin types",
            "builtin types only, such as `str` for Python or `int` in Go",
            "Python decorators, c-like __attribute__",
            "`\\b`",
            "`*`, `+`",
            "`.`",
            "`(`, `)`, `[`, `]`",
            "`|`, `-`",
            'Literal strings: "Hello, world!"',
            'non-regex escapes: "\\t", "\\n"',
            "datetimes within strings, special words within a string, `{}` in format strings",
            '"key" in { "key": "value" }, useful for example in JSON',
            "'c' or similar, in languages that differentiate strings and characters",
            "Literal numbers, both floats and integers",
            "`true`, `false`",
            "Used for XML-like tags",
            "Attribute name in XML-like tags",
            "Delimiters for XML-like tags",
        ),
        allow_alias=True,
    )
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


@proto_enum(DirectEnum(SCIP))
class Severity(IntEnum):
    UnspecifiedSeverity = 0
    Error = 1
    Warning = 2
    Information = 3
    Hint = 4


@proto_enum(DirectEnum(SCIP))
class DiagnosticTag(IntEnum):
    UnspecifiedDiagnosticTag = 0
    Unnecessary = 1
    Deprecated = 2


@proto_enum(
    DirectEnum(
        SCIP,
        description=(
            "Language standardises names of common programming languages that can be used\n "
            "for the `Document.language` field. The primary purpose of this enum is to\n "
            "prevent a situation where we have a single programming language ends up with\n "
            "multiple string representations. For example, the C++ language uses the name\n "
            '"CPP" in this enum and other names such as "cpp" are incompatible.\n Feel free to '
            "send a pull-request to add missing programming languages."
        ),
        value_descriptions=(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            'C++ (the name "CPP" was chosen for consistency with LSP)',
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "https://nickel-lang.org/",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "Internal language for testing SCIP",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "Bash",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            (
                'NextLanguage = 111;\n Steps add a new language:\n 1. Copy-paste the "NextLanguage '
                '= N" line above\n 2. Increment "NextLanguage = N" to "NextLanguage = N+1"\n 3. '
                'Replace "NextLanguage = N" with the name of the new language.\n 4. Move the new '
                "language to the correct line above using alphabetical order\n 5. (optional) Add a "
                "brief comment behind the language if the name is not self-explanatory"
            ),
        ),
    )
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
    DirectMessage(
        SCIP,
        description=(
            "Index represents a complete SCIP index for a workspace this is rooted at a\n "
            "single directory. An Index message payload can have a large memory footprint\n "
            "and it's therefore recommended to emit and consume an Index payload one field\n "
            "value at a time. To permit streaming consumption of an Index payload, the\n "
            "`metadata` field must appear at the start of the stream and must only appear\n "
            "once in the stream. Other field values may appear in any order."
        ),
    )
)
class Index(ProtoModel):
    """Index represents a complete SCIP index for a workspace this is rooted at a
    single directory. An Index message payload can have a large memory footprint
    and it's therefore recommended to emit and consume an Index payload one field
    value at a time. To permit streaming consumption of an Index payload, the
    `metadata` field must appear at the start of the stream and must only appear
    once in the stream. Other field values may appear in any order."""

    metadata: Field[Metadata] = proto_field(
        default=..., number=1, description="Metadata about this index."
    )
    documents: Field[list[Document]] = proto_field(
        default=..., number=2, description="Documents that belong to this index."
    )
    external_symbols: Field[list[SymbolInformation]] = proto_field(
        default=...,
        number=3,
        description=(
            "(optional) Symbols that are referenced from this index but are defined in\n an "
            "external package (a separate `Index` message). Leave this field empty\n if you "
            "assume the external package will get indexed separately. If the\n external "
            "package won't get indexed for some reason then you can use this\n field to "
            "provide hover documentation for those external symbols.\n\nIMPORTANT: When adding "
            "a new field to `Index` here, add a matching\n function in `IndexVisitor` and "
            "update `ParseStreaming`."
        ),
    )


@proto_message(DirectMessage(SCIP))
class Metadata(ProtoModel):
    version: Field[ProtocolVersion] = proto_field(
        default=...,
        number=1,
        description="Which version of this protocol was used to generate this index?",
    )
    tool_info: Field[ToolInfo] = proto_field(
        default=...,
        number=2,
        description="Information about the tool that produced this index.",
    )
    project_root: Field[str] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "URI-encoded absolute path to the root directory of this index. All\n documents in "
            "this index must appear in a subdirectory of this root\n directory."
        ),
    )
    text_document_encoding: Field[TextEncoding] = proto_field(
        default=...,
        number=4,
        description=(
            "Text encoding of the source files on disk that are referenced from\n "
            "`Document.relative_path`. This value is unrelated to the `Document.text`\n field, "
            "which is a Protobuf string and hence must be UTF-8 encoded."
        ),
    )


@proto_message(DirectMessage(SCIP))
class ToolInfo(ProtoModel):
    name: Field[str] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Name of the indexer that produced this index.",
    )
    version: Field[str] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Version of the indexer that produced this index.",
    )
    arguments: Field[list[str]] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Command-line arguments that were used to invoke this indexer.",
    )


@proto_message(
    DirectMessage(
        SCIP, description="Document defines the metadata about a source file on disk."
    )
)
class Document(ProtoModel):
    """Document defines the metadata about a source file on disk."""

    language: Field[str] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "The string ID for the programming language this file is written in.\n The "
            "`Language` enum contains the names of most common programming languages.\n This "
            "field is typed as a string to permit any programming language, including\n ones "
            "that are not specified by the `Language` enum."
        ),
    )
    relative_path: Field[str] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "(Required) Unique path to the text document.\n\n 1. The path must be relative to "
            "the directory supplied in the associated\n    `Metadata.project_root`.\n 2. The "
            "path must not begin with a leading '/'.\n 3. The path must point to a regular "
            "file, not a symbolic link.\n 4. The path must use '/' as the separator, including "
            "on Windows.\n 5. The path must be canonical; it cannot include empty components "
            "('//'),\n    or '.' or '..'."
        ),
    )
    occurrences: Field[list[Occurrence]] = proto_field(
        default=..., number=2, description="Occurrences that appear in this file."
    )
    symbols: Field[list[SymbolInformation]] = proto_field(
        default=...,
        number=3,
        description=(
            'Symbols that are "defined" within this document.\n\n This should include symbols '
            "which technically do not have any definition,\n but have a reference and are "
            "defined by some other symbol (see\n Relationship.is_definition)."
        ),
    )
    text: Field[str] = proto_field(
        default=...,
        number=5,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "(optional) Text contents of this document. Indexers are not expected to\n include "
            "the text by default. It's preferable that clients read the text\n contents from "
            "the file system by resolving the absolute path from joining\n "
            "`Index.metadata.project_root` and `Document.relative_path`. This field\n can be "
            "useful for testing or when working with virtual/in-memory documents."
        ),
    )
    position_encoding: Field[PositionEncoding] = proto_field(
        default=...,
        number=6,
        description=(
            "Specifies the encoding used for source ranges in this Document.\n\n Usually, this "
            "will match the type used to index the string type\n in the indexer's "
            "implementation language in O(1) time.\n - For an indexer implemented in JVM/.NET "
            "language or JavaScript/TypeScript,\n   use UTF16CodeUnitOffsetFromLineStart.\n - "
            "For an indexer implemented in Python,\n   use UTF32CodeUnitOffsetFromLineStart.\n "
            "- For an indexer implemented in Go, Rust or C++,\n   use "
            "UTF8ByteOffsetFromLineStart."
        ),
    )


@proto_message(
    DirectMessage(
        SCIP,
        description=(
            "Symbol is similar to a URI, it identifies a class, method, or a local\n variable. "
            "`SymbolInformation` contains rich metadata about symbols such as\n the docstring.\n"
            "\n Symbol has a standardized string representation, which can be used\n "
            "interchangeably with `Symbol`. The syntax for Symbol is the following:\n ```\n # "
            "(<x>)+ stands for one or more repetitions of <x>\n # (<x>)? stands for zero or "
            "one occurrence of <x>\n <symbol>               ::= <scheme> ' ' <package> ' ' "
            "(<descriptor>)+ | 'local ' <local-id>\n <package>              ::= <manager> ' ' "
            "<package-name> ' ' <version>\n <scheme>               ::= any UTF-8, escape "
            "spaces with double space. Must not be empty nor start with 'local'\n <manager>    "
            "          ::= any UTF-8, escape spaces with double space. Use the placeholder "
            "'.' to indicate an empty value\n <package-name>         ::= same as above\n "
            "<version>              ::= same as above\n <descriptor>           ::= <namespace> "
            "| <type> | <term> | <method> | <type-parameter> | <parameter> | <meta> | <macro>\n"
            " <namespace>            ::= <name> '/'\n <type>                 ::= <name> '#'\n "
            "<term>                 ::= <name> '.'\n <meta>                 ::= <name> ':'\n "
            "<macro>                ::= <name> '!'\n <method>               ::= <name> '(' "
            "(<method-disambiguator>)? ').'\n <type-parameter>       ::= '[' <name> ']'\n "
            "<parameter>            ::= '(' <name> ')'\n <name>                 ::= "
            "<identifier>\n <method-disambiguator> ::= <simple-identifier>\n <identifier>       "
            "    ::= <simple-identifier> | <escaped-identifier>\n <simple-identifier>    ::= "
            "(<identifier-character>)+\n <identifier-character> ::= '_' | '+' | '-' | '$' | "
            "ASCII letter or digit\n <escaped-identifier>   ::= '`' (<escaped-character>)+ "
            "'`', must contain at least one non-<identifier-character>\n <escaped-characters>  "
            " ::= any UTF-8, escape backticks with double backtick.\n <local-id>             "
            "::= <simple-identifier>\n ```\n\n The list of descriptors for a symbol should "
            "together form a fully\n qualified name for the symbol. That is, it should serve "
            "as a unique\n identifier across the package. Typically, it will include one "
            "descriptor\n for every node in the AST (along the ancestry path) between the root "
            "of\n the file and the node corresponding to the symbol.\n\n Local symbols MUST only "
            "be used for entities which are local to a Document,\n and cannot be accessed from "
            "outside the Document."
        ),
    )
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

    scheme: Field[str] = proto_field(
        default=..., number=1, proto_type=ProtoFieldDescriptor.TYPE_STRING
    )
    package: Field[Package] = proto_field(default=..., number=2)
    descriptors: Field[list[Descriptor]] = proto_field(default=..., number=3)


@proto_message(
    DirectMessage(
        SCIP,
        description=(
            "Unit of packaging and distribution.\n\n NOTE: This corresponds to a module in Go "
            "and JVM languages."
        ),
    )
)
class Package(ProtoModel):
    """Unit of packaging and distribution.

    NOTE: This corresponds to a module in Go and JVM languages."""

    manager: Field[str] = proto_field(
        default=..., number=1, proto_type=ProtoFieldDescriptor.TYPE_STRING
    )
    name: Field[str] = proto_field(
        default=..., number=2, proto_type=ProtoFieldDescriptor.TYPE_STRING
    )
    version: Field[str] = proto_field(
        default=..., number=3, proto_type=ProtoFieldDescriptor.TYPE_STRING
    )


@proto_enum(
    DirectEnum(
        SCIP,
        name="Suffix",
        parent=lambda: Descriptor,
        value_descriptions=(
            None,
            (
                "Unit of code abstraction and/or namespacing.\n\n NOTE: This corresponds to a "
                "package in Go and JVM languages."
            ),
            "Use Namespace instead.",
            None,
            None,
            None,
            None,
            None,
            "Can be used for any purpose.",
            None,
            None,
        ),
        allow_alias=True,
    )
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


@proto_message(DirectMessage(SCIP))
class Descriptor(ProtoModel):
    name: Field[str] = proto_field(
        default=..., number=1, proto_type=ProtoFieldDescriptor.TYPE_STRING
    )
    disambiguator: Field[str] = proto_field(
        default=..., number=2, proto_type=ProtoFieldDescriptor.TYPE_STRING
    )
    suffix: Field[DescriptorSuffix] = proto_field(
        default=...,
        number=3,
        description=(
            "NOTE: If you add new fields here, make sure to update the prepareSlot()\n "
            "function responsible for parsing symbols."
        ),
    )


@proto_message(
    DirectMessage(
        SCIP,
        description=(
            "Signature represents the signature of a symbol as it's displayed in API\n "
            "documentation or hover tooltips. It uses a subset of Document's fields with\n the "
            "same field numbers for wire compatibility with older indexes that encoded\n "
            "signatures using the Document message type."
        ),
        reserved_numbers=(1, 3, 6),
    )
)
class Signature(ProtoModel):
    """Signature represents the signature of a symbol as it's displayed in API
    documentation or hover tooltips. It uses a subset of Document's fields with
    the same field numbers for wire compatibility with older indexes that encoded
    signatures using the Document message type."""

    language: Field[str] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description='The language of the signature, e.g. "java", "go", "python".',
    )
    text: Field[str] = proto_field(
        default=...,
        number=5,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description='The text content of the signature, e.g. "void add(int a, int b)".',
    )
    occurrences: Field[list[Occurrence]] = proto_field(
        default=...,
        number=2,
        description=(
            "(optional) Occurrences within the signature text that reference other\n symbols, "
            "enabling hyperlinking of types in the signature. Ranges are\n relative to the "
            "`text` field."
        ),
    )


@proto_enum(
    DirectEnum(
        SCIP,
        name="Kind",
        parent=lambda: SymbolInformation,
        description=(
            "(optional) Kind represents the fine-grained category of a symbol, suitable for "
            "presenting\n information about the symbol's meaning in the language.\n\n For "
            "example:\n - A Java method would have the kind `Method` while a Go function would\n"
            "   have the kind `Function`, even if the symbols for these use the same\n   "
            "syntax for the descriptor `SymbolDescriptor.Suffix.Method`.\n - A Go struct has "
            "the symbol kind `Struct` while a Java class has\n   the symbol kind `Class` even "
            "if they both have the same descriptor:\n   `SymbolDescriptor.Suffix.Type`.\n\n "
            "Since Kind is more fine-grained than Suffix:\n - If two symbols have the same "
            "Kind, they should share the same Suffix.\n - If two symbols have different "
            "Suffixes, they should have different Kinds."
        ),
        value_descriptions=(
            None,
            "A method which may or may not have a body. For Java, Kotlin etc.",
            "For Ruby's attr_accessor",
            None,
            "For Alloy",
            None,
            "For C++",
            "For Lean",
            None,
            None,
            "For C++",
            None,
            None,
            "For Solidity",
            "For Haskell",
            "For C# and F#",
            None,
            None,
            None,
            None,
            "For Dart",
            "For Alloy",
            None,
            None,
            None,
            "For 'get' in Swift, 'attr_reader' in Ruby",
            "For Raku",
            "For Purescript and Lean",
            None,
            None,
            "For Racket",
            "For Lean",
            "For solidity",
            None,
            None,
            "For Ruby",
            (
                "Analogous to 'ThisParameter' and 'SelfParameter', but for languages\n like Go "
                "where the receiver doesn't have a conventional name."
            ),
            "Analogous to 'AbstractMethod', for Go.",
            "For Protobuf",
            "For Dart",
            "For Solidity",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "For Haskell's PatternSynonyms",
            "For Alloy",
            None,
            "Analogous to 'Trait' and 'TypeClass', for Swift and Objective-C",
            "Analogous to 'AbstractMethod', for Swift and Objective-C.",
            "Analogous to 'AbstractMethod', for C++.",
            "For Haskell",
            "'self' in Python, Rust, Swift etc.",
            "For 'set' in Swift, 'attr_writer' in Ruby",
            "For Alloy, analogous to 'Struct'.",
            "For Ruby",
            "Analogous to 'StaticMethod', for Ruby.",
            "Analogous to 'StaticField', for C++",
            "For C#",
            "For C#",
            "For Java, C#, C++ etc.",
            "For C#, TypeScript etc.",
            "For C, C++",
            None,
            None,
            "For Swift",
            "For Lean",
            "For Lean",
            "Method receiver for languages\n 'this' in JavaScript, C++, Java etc.",
            "Analogous to 'Protocol' and 'TypeClass', for Rust, Scala etc.",
            "Analogous to 'AbstractMethod', for Rust, Scala etc.",
            (
                "Data type definition for languages like OCaml which use `type`\n rather than "
                "separate keywords like `struct` and `enum`."
            ),
            None,
            "Analogous to 'Trait' and 'Protocol', for Haskell, Purescript etc.",
            "Analogous to 'AbstractMethod', for Haskell, Purescript etc.",
            "For Haskell",
            None,
            "For C, C++, Capn Proto",
            None,
            "Next = 87;\n Feel free to open a PR proposing new language-specific kinds.",
        ),
    )
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
    DirectMessage(
        SCIP,
        description=(
            "SymbolInformation defines metadata about a symbol, such as the symbol's\n "
            "docstring or what package it's defined it."
        ),
    )
)
class SymbolInformation(ProtoModel):
    """SymbolInformation defines metadata about a symbol, such as the symbol's
    docstring or what package it's defined it."""

    symbol: Field[str] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Identifier of this symbol, which can be referenced from `Occurence.symbol`.\n The "
            "string must be formatted according to the grammar in `Symbol`."
        ),
    )
    documentation: Field[list[str]] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "(optional, but strongly recommended) The markdown-formatted documentation\n for "
            "this symbol. Use `SymbolInformation.signature_documentation` to\n document the "
            "method/class/type signature of this symbol.\n Due to historical reasons, indexers "
            "may include signature documentation in\n this field by rendering markdown code "
            "blocks. New indexers should only\n include non-code documentation in this field, "
            "for example docstrings."
        ),
    )
    relationships: Field[list[Relationship]] = proto_field(
        default=...,
        number=4,
        description="(optional) Relationships to other symbols (e.g., implements, type definition).",
    )
    kind: Field[SymbolInformationKind] = proto_field(
        default=...,
        number=5,
        description=(
            "The kind of this symbol. Use this field instead of\n `SymbolDescriptor.Suffix` to "
            "determine whether something is, for example, a\n class or a method."
        ),
    )
    display_name: Field[str] = proto_field(
        default=...,
        number=6,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "(optional) The name of this symbol as it should be displayed to the user.\n For "
            'example, the symbol "com/example/MyClass#myMethod(+1)." should have the\n display '
            'name "myMethod". The `symbol` field is not a reliable source of\n the display '
            "name for several reasons:\n\n - Local symbols don't encode the name.\n - Some "
            "languages have case-insensitive names, so the symbol is all-lowercase.\n - The "
            "symbol may encode names with special characters that should not be\n   displayed "
            "to the user."
        ),
    )
    signature_documentation: Field[Signature] = proto_field(
        default=...,
        number=7,
        description=(
            "(optional) The signature of this symbol as it's displayed in API\n documentation "
            "or in hover tooltips. For example, a Java method that adds\n two numbers would "
            'have `Signature.language = "java"` and\n `Signature.text = "void add(int a, int '
            'b)"`. The `language` and `text`\n fields are required while `occurrences` can be '
            "optionally included to\n support hyperlinking referenced symbols in the "
            "signature."
        ),
    )
    enclosing_symbol: Field[str] = proto_field(
        default=...,
        number=8,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "(optional) The enclosing symbol if this is a local symbol.  For non-local\n "
            "symbols, the enclosing symbol should be parsed from the `symbol` field\n using "
            "the `Descriptor` grammar.\n\n The primary use-case for this field is to allow "
            "local symbol to be displayed\n in a symbol hierarchy for API documentation. It's "
            "OK to leave this field\n empty for local variables since local variables usually "
            "don't belong in API\n documentation. However, in the situation that you wish to "
            "include a local\n symbol in the hierarchy, then you can use `enclosing_symbol` to "
            'locate the\n "parent" or "owner" of this local symbol. For example, a Java '
            "indexer may\n choose to use local symbols for private class fields while "
            "providing an\n `enclosing_symbol` to reference the enclosing class to allow the "
            "field to\n be part of the class documentation hierarchy. From the perspective of "
            "an\n author of an indexer, the decision to use a local symbol or global symbol\n "
            "should exclusively be determined whether the local symbol is accessible\n outside "
            "the document, not by the capability to find the enclosing\n symbol."
        ),
    )


@proto_message(DirectMessage(SCIP))
class Relationship(ProtoModel):
    symbol: Field[str] = proto_field(
        default=..., number=1, proto_type=ProtoFieldDescriptor.TYPE_STRING
    )
    is_reference: Field[bool] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description=(
            'When resolving "Find references", this field documents what other symbols\n '
            "should be included together with this symbol. For example, consider the\n "
            "following TypeScript code that defines two symbols `Animal#sound()` and\n "
            "`Dog#sound()`:\n ```ts\n interface Animal {\n           ^^^^^^ definition Animal#\n  "
            " sound(): string\n   ^^^^^ definition Animal#sound()\n }\n class Dog implements "
            'Animal {\n       ^^^ definition Dog#, relationships = [{symbol: "Animal#", '
            'is_implementation: true}]\n   public sound(): string { return "woof" }\n          '
            "^^^^^ definition Dog#sound(), references_symbols = Animal#sound(), relationships "
            '= [{symbol: "Animal#sound()", is_implementation:true, is_reference: true}]\n }\n '
            "const animal: Animal = new Dog()\n               ^^^^^^ reference Animal#\n "
            "console.log(animal.sound())\n                    ^^^^^ reference Animal#sound()\n "
            '```\n Doing "Find references" on the symbol `Animal#sound()` should return\n '
            'references to the `Dog#sound()` method as well. Vice-versa, doing "Find\n '
            'references" on the `Dog#sound()` method should include references to the\n '
            "`Animal#sound()` method as well."
        ),
    )
    is_implementation: Field[bool] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description=(
            'Similar to `is_reference` but for "Find implementations".\n It\'s common for '
            "`is_implementation` and `is_reference` to both be true but\n it's not always the "
            "case.\n In the TypeScript example above, observe that `Dog#` has an\n "
            '`is_implementation` relationship with `"Animal#"` but not `is_reference`.\n This '
            'is because "Find references" on the "Animal#" symbol should not return\n "Dog#". '
            'We only want "Dog#" to return as a result for "Find\n implementations" on the '
            '"Animal#" symbol.'
        ),
    )
    is_type_definition: Field[bool] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description='Similar to `references_symbols` but for "Go to type definition".',
    )
    is_definition: Field[bool] = proto_field(
        default=...,
        number=5,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description=(
            'Allows overriding the behavior of "Go to definition" and "Find references"\n for '
            "symbols which do not have a definition of their own or could\n potentially have "
            "multiple definitions.\n\n For example, in a language with single inheritance and "
            "no field overriding,\n inherited fields can reuse the same symbol as the ancestor "
            "which declares\n the field. In such a situation, is_definition is not needed.\n\n "
            "On the other hand, in languages with single inheritance and some form\n of "
            "mixins, you can use is_definition to relate the symbol to the\n matching symbol "
            "in ancestor classes, and is_reference to relate the\n symbol to the matching "
            "symbol in mixins.\n\nUpdate registerInverseRelationships on adding a new field "
            "here."
        ),
    )


@proto_message(
    DirectMessage(
        SCIP,
        description=(
            "SingleLineRange represents a half-open [start, end) range within a single line.\n\n"
            " Line numbers and characters are always 0-based. Make sure to increment them\n "
            "before displaying in an editor-like UI because editors conventionally use\n "
            "1-based numbers. The `character` values are interpreted based on the\n "
            "`PositionEncoding` for the enclosing Document."
        ),
    )
)
class SingleLineRange(ProtoModel):
    """SingleLineRange represents a half-open [start, end) range within a single line.

    Line numbers and characters are always 0-based. Make sure to increment them
    before displaying in an editor-like UI because editors conventionally use
    1-based numbers. The `character` values are interpreted based on the
    `PositionEncoding` for the enclosing Document."""

    line: Field[int] = proto_field(
        default=..., number=1, proto_type=ProtoFieldDescriptor.TYPE_INT32
    )
    start_character: Field[int] = proto_field(
        default=..., number=2, proto_type=ProtoFieldDescriptor.TYPE_INT32
    )
    end_character: Field[int] = proto_field(
        default=..., number=3, proto_type=ProtoFieldDescriptor.TYPE_INT32
    )


@proto_message(
    DirectMessage(
        SCIP,
        description=(
            "MultiLineRange represents a half-open [start, end) range spanning multiple "
            "lines.\n\n Line numbers and characters are always 0-based. Make sure to increment "
            "them\n before displaying in an editor-like UI because editors conventionally use\n "
            "1-based numbers. The `character` values are interpreted based on the\n "
            "`PositionEncoding` for the enclosing Document.\n\n Producers SHOULD use "
            "`SingleLineRange` when `start_line == end_line` to keep\n indexes compact, but "
            "consumers MUST accept multi-line encoding even when the\n range happens to fit on "
            "a single line."
        ),
    )
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

    start_line: Field[int] = proto_field(
        default=..., number=1, proto_type=ProtoFieldDescriptor.TYPE_INT32
    )
    start_character: Field[int] = proto_field(
        default=..., number=2, proto_type=ProtoFieldDescriptor.TYPE_INT32
    )
    end_line: Field[int] = proto_field(
        default=..., number=3, proto_type=ProtoFieldDescriptor.TYPE_INT32
    )
    end_character: Field[int] = proto_field(
        default=..., number=4, proto_type=ProtoFieldDescriptor.TYPE_INT32
    )


@proto_message(
    DirectMessage(
        SCIP,
        description=(
            "Occurrence associates a source position with a symbol and/or highlighting\n "
            "information.\n\n If possible, indexers should try to bundle logically related "
            "information\n across occurrences into a single occurrence to reduce payload "
            "sizes.\n\n Range encoding:\n\n An Occurrence carries its source range in one of two "
            "ways: the deprecated\n `range` field (a `repeated int32` packed encoding kept for "
            "backward\n compatibility), or one of the typed alternatives in the `typed_range`\n "
            "oneof. New producers SHOULD set `typed_range` and SHOULD NOT set the\n deprecated "
            "`range` field. The same rule applies to `enclosing_range` and\n "
            "`typed_enclosing_range`.\n\n When both encodings are present on the same "
            "Occurrence, `typed_range` takes\n precedence over `range` (likewise "
            "`typed_enclosing_range` over\n `enclosing_range`). Producers that set both forms "
            "MUST keep them\n semantically equivalent. Consumers SHOULD prefer the typed form "
            "when\n available and fall back to the `repeated int32` form otherwise."
        ),
    )
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

    range: Field[list[int]] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_INT32,
        deprecated=True,
        description=(
            "Deprecated: Use `single_line_range` or `multi_line_range` instead.\n\n Half-open "
            "[start, end) range. Must be exactly three or four elements:\n - Three elements: "
            "`[startLine, startCharacter, endCharacter]` (single-line)\n - Four elements: "
            "`[startLine, startCharacter, endLine, endCharacter]`\n\n The end line of a "
            "three-element range is inferred to equal the start line.\n\n Historical note: the "
            "original draft of this schema had a `Range` message\n type with `start` and `end` "
            "fields of type `Position`, mirroring LSP.\n Benchmarks revealed that this "
            "encoding was inefficient and that we could\n reduce the total payload size of an "
            "index by 50% by using `repeated int32`\n instead. However, the lack of type "
            "safety led to the introduction of\n `single_line_range` and `multi_line_range` as "
            "typed alternatives; the\n typed encoding's per-index size overhead is small "
            "(single-digit percent)\n because ranges are only a fraction of a typical index "
            "payload."
        ),
    )
    single_line_range: Field[SingleLineRange | None] = proto_field(
        default=None,
        number=8,
        oneof=Oneof("typed_range"),
        description="Range spanning a single line.",
    )
    multi_line_range: Field[MultiLineRange | None] = proto_field(
        default=None,
        number=9,
        oneof=Oneof("typed_range"),
        description="Range spanning multiple lines.",
    )
    symbol: Field[str] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "(optional) The symbol that appears at this position. See\n "
            "`SymbolInformation.symbol` for how to format symbols as strings."
        ),
    )
    symbol_roles: Field[int] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_INT32,
        description=(
            "(optional) Bitset containing `SymbolRole`s in this occurrence.\n See "
            "`SymbolRole`'s documentation for how to read and write this field."
        ),
    )
    override_documentation: Field[list[str]] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "(optional) CommonMark-formatted documentation for this specific range. If\n "
            "empty, the `Symbol.documentation` field is used instead. One example\n where this "
            "field might be useful is when the symbol represents a generic\n function (with "
            "abstract type parameters such as `List<T>`) and at this\n occurrence we know the "
            "exact values (such as `List<String>`).\n\n This field can also be used for "
            "dynamically or gradually typed languages,\n which commonly allow for "
            "type-changing assignment."
        ),
    )
    syntax_kind: Field[SyntaxKind] = proto_field(
        default=...,
        number=5,
        description="(optional) What syntax highlighting class should be used for this range?",
    )
    diagnostics: Field[list[Diagnostic]] = proto_field(
        default=...,
        number=6,
        description="(optional) Diagnostics that have been reported for this specific range.",
    )
    enclosing_range: Field[list[int]] = proto_field(
        default=...,
        number=7,
        proto_type=ProtoFieldDescriptor.TYPE_INT32,
        deprecated=True,
        description=(
            "Deprecated: Use `typed_enclosing_range` instead.\n\n Uses the same `repeated "
            "int32` encoding as the deprecated `range` field."
        ),
    )
    single_line_enclosing_range: Field[SingleLineRange | None] = proto_field(
        default=None,
        number=10,
        oneof=Oneof("typed_enclosing_range"),
        description="Enclosing range spanning a single line.",
    )
    multi_line_enclosing_range: Field[MultiLineRange | None] = proto_field(
        default=None,
        number=11,
        oneof=Oneof("typed_enclosing_range"),
        description="Enclosing range spanning multiple lines.",
    )


@proto_message(
    DirectMessage(
        SCIP,
        description=(
            "Represents a diagnostic, such as a compiler error or warning, which should be\n "
            "reported for a document."
        ),
    )
)
class Diagnostic(ProtoModel):
    """Represents a diagnostic, such as a compiler error or warning, which should be
    reported for a document."""

    severity: Field[Severity] = proto_field(
        default=...,
        number=1,
        description="Should this diagnostic be reported as an error, warning, info, or hint?",
    )
    code: Field[str] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="(optional) Code of this diagnostic, which might appear in the user interface.",
    )
    message: Field[str] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Message of this diagnostic.",
    )
    source: Field[str] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "(optional) Human-readable string describing the source of this diagnostic, e.g.\n "
            "'typescript' or 'super lint'."
        ),
    )
    tags: Field[list[DiagnosticTag]] = proto_field(default=..., number=5)


SCIP_PACKAGE = ProtoPackage(
    file=ProtoFile(
        "scip/scip.proto",
        SCIP,
        options=(
            ProtoOption("go_package", "github.com/scip-code/scip/bindings/go/scip/"),
            ProtoOption("java_multiple_files", True),
            ProtoOption("java_outer_classname", "ScipProto"),
            ProtoOption("java_package", "org.scip_code.scip"),
        ),
        upstream_release="v0.9.0",
        upstream_schema="https://github.com/scip-code/scip/blob/v0.9.0/scip.proto",
    ),
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
    services=(),
)
