# AI Agent Instructions

Don’t fight errors! Whenever you encounter the same error twice, research the web and find 3-5 possible ways to fix it. Then choose the most efficient solution and implement it.

## `gql` and `gql-planner`

The `gleaph-gql` and `gleaph-gql-planner` crates contain the name Gleaph as an identifier, but in reality, they should be general-purpose crates for GQL (ISO/IEC 39075).

Therefore, Gleaph(or ICP)-specific implementations or mentions should not encroach upon these crates.

## Internet Computer

Tested implementation patterns for ICP development are available as agent skills.
Before writing any ICP code, fetch the skills index and remember each skill's name and description:
https://skills.internetcomputer.org/.well-known/skills/index.json

When a task matches a skill's description, fetch its content on-demand from:
https://skills.internetcomputer.org/.well-known/skills/{name}/SKILL.md

Skills contain correct dependency versions, configuration formats, and common pitfalls that prevent build failures.
Always prefer skill guidance over general documentation when both cover the same topic.
