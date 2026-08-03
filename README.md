# rift - agentic development toolkit

`rift` is an MCP assistant that provides capabilities for agentic-driven, typesafe development.
It gives agents tools and resources to read, search, discover and edit codebases at scale and with high token efficiency.

## Installation

<TODO>

## Usage

To use `rift` in a specific project with a selected agent, navigate to the project directory and run:

```bash
rift init
```

## How it works?

Rift provides a skill and an MCP server with tools and resources. Agents can use these tools to read, search, discover and edit codebases in efficient, compiler-driven and validated way.

### Resources

<TODO>

### Tools

<TODO>

## Why `rift`?

There are many tools that provide code search and editing capabilities, but:

- most of them are using only tree sitter which is enough to parse code, but doesn't provide typesafe introspection
- they're primarily read-only, while rift has a strong focus on compiler-validated editing and code generation
