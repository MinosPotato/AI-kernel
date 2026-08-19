## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).

## Approach
- Read existing files before writing. Don't re-read unless changed.
- Thorough in reasoning, concise in output.
- Skip files over 100KB unless required.
- No sycophantic openers or closing fluff.
- No emojis or em-dashes.
- Do not guess APIs, versions, flags, commit SHAs, or package names. Verify by reading code or docs before asserting.


## Project

This is an AI harness/kernel. The goal is a secure, reliable, maintainable, production-quality system.

## How to Work

- Inspect the existing code and architecture before making changes.
- Make architectural decisions yourself based on the repository and requirements.
- Do not invent unnecessary architecture, abstractions, or dependencies.
- Preserve working functionality unless requirements require changing it.
- Prefer simple, incremental, testable changes.
- Do not ask for approval for normal implementation decisions.

## Requirements

- Read `REQUIREMENTS.md` if it exists.
- Requirements describe what the system must do, not necessarily how.
- Do not silently ignore or change requirements.

## Security

Treat the harness as security-sensitive.

Pay particular attention to:
- filesystem permissions
- command execution
- process isolation
- path traversal
- credentials and secrets
- network access
- privilege boundaries

Use least privilege and fail closed on security-sensitive errors.

Never expose or commit secrets.

## Verification

After meaningful changes:

1. Run relevant tests/builds/checks.
2. Fix failures you caused.
3. Review the implementation for correctness, security, and regressions.

Do not consider code complete merely because it compiles.

## Autonomous Work

When working autonomously:

- Inspect the current state before each task.
- Choose the highest-priority incomplete work.
- Implement it completely.
- Test it.
- Continue to the next useful task.
- Do not create artificial work just to keep going.
- Stop when the requirements are genuinely satisfied and verified.

## Token Efficiency

- Keep communication concise.
- Avoid repeating information already in the repository.
- Prefer targeted code inspection over rereading large files.
- Do the work instead of explaining what could be done.
- Keep durable project knowledge in repository files, not conversation history.

## Git

Keep changes coherent and inspect diffs before committing.

Never commit secrets or temporary/debug files.

## Core Principle

Produce correct, secure, tested software, not merely more code.
