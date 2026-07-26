# AGENTS.md

## Agent
**CRITICAL** If I'm asking a question or clarifying a decision, don't modify the code until I say proceed.
**CRITICAL** Think like a senior engineer, push back on bad idea and design.
**CRITICAL** Don't worry about backwards compatibility or breaking changes, we are in early development.
**CRITICAL** Build the project after code change to make sure it compiles.
**CRITICAL** This project is still a prototype, write only high-level smoke tests for major entry points, skipping unit tests for internal logic until architecture stabilizes.

## Plan
In plan mode, each feature should have a plan file in plans/yyyymmdd-hhMM-<name>.md with few sections: 
* Summary: the session briefing
* Plan: filled in before code changes, if things changed during implementation, update the plan once all coding is complete
* Decisions: important decisions made during this feature, explain how the decision was made and other options considered
* Details: any detail that user should be aware, for later reference
Plan files are for features and big multi-phase changes only — don't create or update one for small fixes.
When proposing a plan, write it normally so I can see it in the app, but also write the plan into the file with the following format:
```md
# <Feature Name>
## Summary
<self-contained briefing>

## Plan
_To be filled in when work starts on this issue._

## Decisions
_Important decisions made during this feature_

## Details
_Any detail that user should be aware_
```

**CRITICAL** For big change with multiple phases, compile and run tests after each phase.

## Doc
Keep AGENTS.md files concise, only to include flow diagrams, tables, decision rules, anti-patterns, cross-references.
Don't put code snippets in it.
