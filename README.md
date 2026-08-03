# rift - agentic development toolkit

`rift` is an MCP assistant that provides capabilities for agentic-driven, typesafe development.
It gives agents tools and resources to read, search, discover and edit codebases at scale and with high token efficiency.

## Installation

On MacOS and Linux, you can install `rift` using the following command:

```bash
curl -fsSL https://volar.sh/rift/install.sh | sh
```

For Windows, you can install `rift` using the following command:

```powershell
iwr -useb https://volar.sh/rift/install.ps1 | iex
```

## Usage

To use `rift` with a specific project, navigate to the project directory and run:

```bash
rift init --agent <agent_name>
```

Where `<agent_name>` is the name of the agent you want to use for development, for example `rift init --agent claude`.

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
