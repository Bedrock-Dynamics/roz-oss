---
name: create-skill
description: Guide the user through creating a new roz skill definition
kind: ai
version: "1.0.0"
tags: [meta, authoring]
parameters: []
safety: null
environment_constraints: []
stream_requirements: []
success_criteria:
  - A valid skill markdown file is written to the skills/ directory
  - The skill has valid YAML frontmatter with all required fields
allowed_tools: [file_write, file_read]
---

You are helping the user create a new roz skill. A skill is a markdown file with YAML frontmatter.

## Required frontmatter fields:
- `name`: kebab-case identifier (e.g. `waypoint-mission`)
- `description`: one-line summary of what the skill does
- `kind`: either `ai` (LLM-driven) or `execution` (deterministic)
- `version`: semver string (start with `1.0.0`)
- `tags`: list of category tags

## Optional frontmatter fields:
- `parameters`: list of typed parameters the skill accepts
- `safety`: override safety limits (max_velocity, max_force, require_confirmation, excluded_zones)
- `environment_constraints`: conditions that must hold before the skill runs
- `stream_requirements`: data streams the skill needs access to
- `success_criteria`: how to determine the skill completed successfully
- `allowed_tools`: restrict which tools the skill can use

## Process:
1. Ask the user what the skill should do
2. Determine the appropriate `kind` (ai for LLM reasoning tasks, execution for deterministic operations)
3. Identify parameters the skill needs
4. Write the skill file to `skills/{name}.md`
5. Validate the frontmatter parses correctly

## Example skill body (after frontmatter):
The body is a markdown prompt that instructs the agent how to execute the skill.
It should include step-by-step instructions, safety considerations, and expected outcomes.
