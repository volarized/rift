DOCUMENT_METADATA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "$id": "https://volar.sh/rift/protocol/mcp.json",
    "title": "Rift MCP surface",
    "description": "JSON values accepted and returned by the Rift MCP server. Each definition carries the Protobuf "
    "identity used after rift-mcp decodes the request.",
    "rift:entryPoints": {
        "description": "The schema's entry points. Every other definition is reachable from at least one "
        "of these seams; shared vocabulary such as SymbolId and Address is deliberately "
        "reachable from several.",
        "mcp.tools": {
            "tree": {
                "rpc": "rift.mcp.Rift/Tree",
                "description": "Lists the project tree at one snapshot. Rift derives "
                "directories from Git paths and answers without starting a "
                "language adapter. A depth of one is an `ls`; an unbounded "
                "depth with path selectors is a recursive tree, glob, or "
                "file find.",
                "params": {"$ref": "#/$defs/TreeParams"},
                "result": {"$ref": "#/$defs/TreeResult"},
            },
            "outline": {
                "rpc": "rift.mcp.Rift/Outline",
                "description": "Reads the compiler-owned declaration structure of one "
                "file. Results preserve source nesting and carry "
                "semantic coverage, so an empty outline distinguishes "
                "an empty file from an unsupported language.",
                "params": {"$ref": "#/$defs/OutlineParams"},
                "result": {"$ref": "#/$defs/OutlineResult"},
            },
            "search": {
                "rpc": "rift.mcp.Rift/Search",
                "description": "Ranked lookup of symbols, leaves and files. A lexical "
                "query finds names or text; a structured filter walks "
                "compiler facts. Path selectors narrow either form. "
                "Every page carries its total order, continuation "
                "cursor, and coverage.",
                "params": {"$ref": "#/$defs/SearchParams"},
                "result": {"$ref": "#/$defs/SearchResult"},
            },
            "match": {
                "rpc": "rift.mcp.Rift/Match",
                "description": "Finds byte ranges. Rift runs literal and "
                "regular-expression matching over every UTF-8 file. The "
                "owning compiler parses structural patterns because it "
                "defines the language's syntax. Every hit carries a key "
                "an edit can address.",
                "params": {"$ref": "#/$defs/MatchParams"},
                "result": {"$ref": "#/$defs/MatchResult"},
            },
            "actions": {
                "rpc": "rift.mcp.Rift/Actions",
                "description": "Asks a compiler what it can offer at one address: the "
                "fixes and refactors it would suggest there. Each "
                "result carries the snapshot and adapter token needed "
                "for resolution. Rift refuses a token after its "
                "snapshot moves. Call this tool after a diagnostic or "
                "before a refactor.",
                "params": {"$ref": "#/$defs/ActionsParams"},
                "result": {"$ref": "#/$defs/ActionsResult"},
            },
            "apply": {
                "rpc": "rift.mcp.Rift/Apply",
                "description": "Builds, refreshes, or publishes a deterministic "
                "candidate. Preview retains each request with its checked "
                "preconditions, exact edits, semantic effects, guarantee "
                "evidence, diff, validation, and confirmations. Refresh "
                "repeats that contract on a newer base. Publish replays "
                "it and advances the accepted ref when the retained plan "
                "still agrees, fresh validation passes, and the base "
                "still matches.",
                "params": {"$ref": "#/$defs/ApplyParams"},
                "result": {"$ref": "#/$defs/ApplyResult"},
            },
            "persist": {
                "rpc": "rift.mcp.Rift/Persist",
                "description": "Materializes an accepted commit into the session "
                "worktree. Each requested path reports its own outcome, "
                "so drift, sparse checkout rules, nested repositories, "
                "and deletion policy remain visible to the caller.",
                "params": {"$ref": "#/$defs/PersistParams"},
                "result": {"$ref": "#/$defs/PersistResult"},
            },
        },
        "mcp.resources": {
            "repository": {
                "description": "What this workspace can do: which languages "
                "have adapters and what each supports, the state "
                "answers resolve against, the limits a request "
                "has to stay inside, and which conformance tier "
                "is in force. Read it before the first call — "
                "the tools and resources listed here are the "
                "ones this workspace actually serves.",
                "template": {"$ref": "#/$defs/ResourceTemplate"},
                "uri": {"$ref": "#/$defs/RepositoryResourceUri"},
                "link": {"$ref": "#/$defs/ResourceLink"},
            },
            "symbol": {
                "description": "Everything Rift holds about one symbol at one "
                "state: the declaration, every leaf that writes it, "
                "its types, its edges and its diagnostics. Read it "
                "before a rename or a delete, where the references "
                "matter as much as the declaration.",
                "template": {"$ref": "#/$defs/ResourceTemplate"},
                "uri": {"$ref": "#/$defs/SymbolId"},
                "link": {"$ref": "#/$defs/ResourceLink"},
            },
            "diff": {
                "description": "What changed between two revisions, a page of files "
                "at a time: the entry on each side and the edits "
                "between them. Git answers it without a compiler, so "
                "it works for a language Rift has no adapter for.",
                "template": {"$ref": "#/$defs/ResourceTemplate"},
                "uri": {"$ref": "#/$defs/DiffId"},
                "link": {"$ref": "#/$defs/ResourceLink"},
            },
            "file": {
                "description": "One file at one revision: the entry Git records, its "
                "language ownership, and one bounded content range. "
                "The next URI continues a large regular file.",
                "template": {"$ref": "#/$defs/ResourceTemplate"},
                "uri": {"$ref": "#/$defs/FileResourceUri"},
                "link": {"$ref": "#/$defs/ResourceLink"},
            },
            "preview": {
                "description": "The complete retained plan behind an apply "
                "preview: its input changes, concrete edits, "
                "candidate diff, compiler and validator evidence, "
                "and confirmations. Read it before publication when "
                "the tool result was bounded to a summary.",
                "template": {"$ref": "#/$defs/ResourceTemplate"},
                "uri": {"$ref": "#/$defs/PreviewResourceUri"},
                "link": {"$ref": "#/$defs/PreviewResourceLink"},
            },
        },
        "mcp.error": {"$ref": "#/$defs/ErrorData"},
        "mcp.resources.read": {
            "rpc": "rift.mcp.Rift/ReadResource",
            "params": {"$ref": "#/$defs/ResourceReadParams"},
            "result": {"$ref": "#/$defs/ResourceReadResult"},
        },
    },
    "rift:axes": {
        "description": "How definitions map to axes. Identifiers are pinned before each group closes over "
        "references. For example, `Leaf` can refer to `SymbolId` while the latter stays "
        "Semantic. `holds` pins a definition. `residualOf` assigns the unclaimed definitions in "
        "one document. The adapter service declares its own groups.",
        "groups": [
            {
                "name": "Versioning",
                "summary": "Which state you are looking at: what you asked for, and what it resolved to.",
                "identifiedBy": [
                    "Revision",
                    "Snapshot",
                    "Commit",
                    "Worktree",
                    "DiffId",
                    "PreviewId",
                ],
            },
            {
                "name": "Filesystem",
                "summary": "Where it is written: files, the leaves of their syntax trees, ranges of bytes, "
                "and the changes proposed to them.",
                "identifiedBy": [
                    "FileId",
                    "ProjectPath",
                    "File",
                    "ProjectEntry",
                    "Leaf",
                    "LeafId",
                    "TextRange",
                    "SourceSpan",
                    "Edit",
                    "TextEdit",
                    "LeafFacet",
                    "LeafRegion",
                    "RegionRole",
                    "FileChange",
                    "OriginMapping",
                    "PersistOutcome",
                    "Capture",
                    "StructuralMatchRanges",
                    "LanguageRegion",
                ],
            },
            {
                "name": "Semantic",
                "summary": "What the compiler knows: symbols, what they carry, and how they connect.",
                "identifiedBy": [
                    "SymbolId",
                    "Symbol",
                    "Relationship",
                    "Signature",
                    "TypeExpression",
                    "Documentation",
                    "ExactKind",
                    "SymbolFacet",
                    "SymbolOrigin",
                    "LanguageId",
                ],
            },
            {
                "name": "Discovery",
                "summary": "Caller-written filters and match queries used to find code before Rift produces "
                "an answer.",
                "identifiedBy": [
                    "Filter",
                    "FieldFilter",
                    "RelationFilter",
                    "PathSelector",
                    "PathPattern",
                    "MatchQuery",
                    "TextQuery",
                    "StructuralQuery",
                    "StructuralCaptureConstraint",
                    "CaptureName",
                ],
            },
            {
                "name": "Reachability",
                "summary": "How much of the answer Rift could actually see, and what the compiler said on "
                "the way.",
                "identifiedBy": [
                    "Coverage",
                    "CoverageScope",
                    "SemanticCoverage",
                    "FactFamily",
                    "Diagnostic",
                    "DiagnosticRelated",
                    "Severity",
                    "ValidationReport",
                    "CandidateValidation",
                    "ValidatorResult",
                    "ValidatorOutput",
                ],
            },
            {
                "name": "Operations",
                "summary": "What you can do to the code, and the identities that pin a discovery to the "
                "state it was made in.",
                "identifiedBy": [
                    "Address",
                    "ActionDescriptor",
                    "ActionKind",
                    "MatchKey",
                    "ActionKey",
                    "FormattingPolicy",
                    "MatchCardinality",
                    "OperationScope",
                    "SafeDeletePolicy",
                    "SignatureChange",
                    "RenameArguments",
                    "MoveArguments",
                    "SafeDeleteArguments",
                    "ChangeSignatureArguments",
                    "ArgumentContract",
                    "OperationVerifier",
                    "PreconditionValue",
                    "OperationPrecondition",
                    "OperationBlocker",
                    "OperationEffect",
                    "GuaranteeKind",
                    "GuaranteeEvidence",
                    "ConfirmationRequirement",
                    "Change",
                    "DirectChange",
                    "PatchChange",
                    "ActionChange",
                    "RewriteChange",
                    "RevertChange",
                    "ResolvedChange",
                    "ActionSupport",
                    "SandboxedValidator",
                ],
            },
            {
                "name": "Protocol",
                "summary": "Shared contract machinery: protocol version, content identities, and extension "
                "namespaces.",
                "holds": [
                    "ProtocolVersion",
                    "Digest",
                    "Extensions",
                    "ExtensionKey",
                    "ExtensionValue",
                ],
                "residualOf": "core",
            },
            {
                "name": "MCP",
                "summary": "Shapes that exist only on the agent-facing surface: parameters, results, "
                "resource payloads.",
                "residualOf": "mcp",
            },
        ],
    },
}
