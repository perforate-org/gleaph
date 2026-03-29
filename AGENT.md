# AI Agent Instructions

Tested implementation patterns for ICP development are available as agent skills.
Before writing any ICP code, fetch the skills index and remember each skill's name and description:
https://skills.internetcomputer.org/.well-known/skills/index.json

When a task matches a skill's description, fetch its content on-demand from:
https://skills.internetcomputer.org/.well-known/skills/{name}/SKILL.md

Skills contain correct dependency versions, configuration formats, and common pitfalls that prevent build failures.
Always prefer skill guidance over general documentation when both cover the same topic.
