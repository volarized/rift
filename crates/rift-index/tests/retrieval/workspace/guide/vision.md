# Vision

Rift answers a question about a codebase by ranking declarations, not files.

The impact radius of one change is the set of declarations that break when it
lands. A reader who asks for that set wants an ordered answer, not a grep.

Two indexes answer: the project's own, and the packages it depends on.
