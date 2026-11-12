---
name: create-pr
description: Draft a short, plain PR title and description for the current branch, and open the PR if asked. Use when the user wants a PR description written or wants to open a PR.
---

# Create PR

Write a PR for the current branch. Keep it as short as it can be while still making sense.

## 1. Understand the change

Run `git log main..HEAD` and `git diff main...HEAD`. Base the text on the real diff, not just commit messages. Cover the whole branch, not only the last commit.

## 2. Write it

- Terse and no-nonsense. Shorter is better. If a sentence can go, cut it.
- Simple words and short sentences. A person should understand it in one quick read.
- Say what changed and why, the way you'd tell a teammate. Not a changelog, not an essay.
- No jargon for its own sake. Use a technical term only when it is the clearest word.
- No filler, no stock phrases, no restating the obvious. No em dashes, en dashes, or semicolons. Avoid parentheses.
- Do not mention tests, lint, or formatting. That is assumed. Only talk about testing or validation when it is the point of the PR.
- No section headers, bullet walls, or "Summary / Changes / Notes" scaffolding unless the PR really has several unrelated parts. A few plain lines or a short bullet list is enough.
- If the PR closes an issue, add `Closes #NNN` on its own line, with no trailing period.

Title: one short line, imperative, under about 70 characters.

## 3. Draft vs. open

Default to drafting only. Print the title and body in a fenced block and stop.

Only push and open the PR if the user clearly asks (for example "open a PR", "create the PR", "submit it"). Then:
- Push the branch if needed with `git push -u origin <branch>`.
- Run `gh pr create --title "..." --body "..."` with the drafted text.
- Print the PR URL.

Never add "Generated with Claude Code" or any similar attribution suffix to the title or body. End the body with the last real sentence.
