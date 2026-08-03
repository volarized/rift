# greif - agentic development toolkit

`greif` is an MCP assistant that provides capabilities for agentic-driven, typesafe development.
It gives agents tools and resources to read, search, discover and edit codebases at scale and with high token efficiency.

## Installation

On MacOS and Linux, you can install `greif` using the following command:

```bash
curl -fsSL https://volar.sh/greif/install.sh | sh
```

For Windows, you can install `greif` using the following command:

```powershell
iwr -useb https://volar.sh/greif/install.ps1 | iex
```

## Usage

To use `greif` with a specific project, navigate to the project directory and run:

```bash
greif init --agent <agent_name>
```

Where `<agent_name>` is the name of the agent you want to use for development, for example `greif init --agent claude`.

## How it works?

Greif provides a skill and an MCP server with tools and resources. Agents can use these tools to read, search, discover and edit codebases in efficient, compiler-driven and validated way.

### Resources

<TODO>

### Tools

<TODO>

## Why `greif`?

There are many tools that provide code search and editing capabilities, but:

- most of them are using only tree sitter which is enough to parse code, but doesn't provide typesafe introspection
- they're primarily read-only, while greif has a strong focus on compiler-validated editing and code generation
