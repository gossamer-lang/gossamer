# Contributing to Gossamer

Thanks for your interest.

## LLM Policy

LLM code and analysis is welcome. It is held to the same standard
as manually written code, analysis, or tickets.

## Github Issues

Github issues must:
  
- Be one issue per bug/enhancement. (Unless the additional bug/enhancement 
is connected in a meaningful way. Use your judgement.)

- Contain a clear and concise summary. No walls of text.

- Be simple to reproduce. Either a clear series of steps, or attached files.
A small github repo is also acceptable. Files that require editing to 
reproduce the issue are not accepted.

## Before you open a PR

- Read `SPEC.md` (language specification) and `GUIDELINES.md` - the
  project style guide. CI enforces every rule in the style guide. No
  exceptions without a written justification in the PR.

## Local checks

Use `quick-check.sh` for quick checks during development.

Before submitting , run `full-check.sh`.

## Commit messages

One logical change per commit. Imperative subject line under 72
characters. Body wraps at 72. No emojis.

## Licensing

Contributions are licensed under Apache-2.0. By opening a PR you agree
to license your contribution under those terms.
