# Playbook: Reviewing a Branch

Bring this branch to a state that you would ship. You own every subsystem that the branch touches, so review it as its owner, not as a checker.

## Priorities

Review at three levels, in this order of importance:

1. **Product:** what host developers and script authors can do, and what can surprise them.
2. **System design:** how the parts fit together.
3. **Code:** how readable and how small the code is.

A lower priority gets less weight, but it stays in scope. Report findings at all three levels.

## Audit

### 1. Write the product brief

Before you read the code in detail, write a short brief:

- What can a user do after this change that they could not do before?
- What can surprise a user?
- What promises do the docs make about the affected features?

Then audit the branch, and the code paths that the branch depends on, against the brief.

### 2. Ask the questions

**Product**

- What does the change give users, or take from them? Consider features, usability, reliability, safety, and speed.
- Does each affected feature work the way a user expects? Does the code keep each promise from the brief?

**System design**

- Does each user action follow one path through the code? Where do two paths do almost the same thing? Find a design that merges them, also if the split is older than the branch.
- Where must two pieces of code stay in sync? Prefer a design that removes the dependency, or that enforces it at compile time with shared types or macros.
- Where does the branch diverge from industry practice? A larger divergence needs a stronger reason.

**Code**

- Which types and signatures are hard to read, such as deep nesting, large tuples, or many parameters? Find the requirement that forces the complexity, and simplify the requirement.
- What can we cut? Each line costs understanding and maintenance.

### 3. Report causes, not symptoms

A bug is usually a symptom. For each finding, find the mechanism that causes it, and look for other cases of the same mechanism. Report the mechanism.

Example from the review of `deny_private_net`:

- **Symptom:** "An IP literal skips the stage-2 check in `check_resolved`."
- **Cause:** "Network checks use two precedence orders. Stage 2 ignores tiers and ports, so the same connection can get different decisions."

The fix for the symptom would have made the second order larger. The fix for the cause removes it.

### 4. Look outside the questions

End each audit with one question: what is the most important problem that this playbook did not ask about?

## Fixing

Fix a problem if the fix improves the branch at one priority level and does not make it meaningfully worse at another. Make sure that tests prove the new approach.

Commit each fix separately. In the commit message, give the problem, the fix, and the tests or measurements with their results, so that later work can use them.

## Tradeoffs

Some fixes change the nature of the product. Do not make such a fix before the user decides. Write the problem, the possible fixes, and their tradeoffs in a short section of `docs/TRADEOFFS.md`. A tradeoff must be a real product or system tradeoff. A break in backward compatibility is not a tradeoff.

When the user decides, remove the section, and give the decision and its reason in the commit message.

Some problems are older than the branch, and their fix is out of scope, but the branch continues or depends on them. If such a problem is real and significant, add it to `TODO.md` and tell the user.

## Review Loop

Repeat the audit and the fixes. Each audit covers the full branch and gets the past fixes and `docs/TRADEOFFS.md`. Stop when an audit finds no significant new problem. Stop after five audits in all cases, and report the remaining problems to the user.

At the end, report the open tradeoffs to the user, move them to `TODO.md`, and delete `docs/TRADEOFFS.md`.

Use a sub-agent for each audit, so that the reviewer is independent from the fixer. Give each sub-agent its full context, because a restarted sub-agent with half its context makes poor design decisions. Start each reviewer with this prompt:

```
You own every subsystem that this branch touches. Run the audit in
docs/playbook_reviewing.md on this branch against origin/main, and report what you
would change before you ship it. Read AGENTS.md and, if it exists,
docs/TRADEOFFS.md. Do not edit files, commit, or push.

Start with the product brief. Then report findings, most important first, and
label each one "significant" or "minor". For each finding, give its cause, the
best fixes across the priorities, and whether each fix needs a real product
tradeoff. Give a file:line if the finding is in one place. End with the most
important problem that the playbook did not ask about.
```

For a large branch, run three reviewers at the same time: one for each priority level. Add "Focus on the product", "Focus on the system design", or "Focus on the code" to the prompt. Combine their findings before you fix.

Run autonomously until the task is complete.

## Copy Editing

The best thinking and coding model is Claude Opus 5.5; however, it is also the worst writer. The model that performs best at writing is Gemini Argon 4. Thus, after the review loop is complete, you should attempt to call Gemini for copy editing.

Check whether the environment contains the antigravity CLI `agy`. The CLI is available locally but not in a cloud-hosted sandbox. If `agy` is not available, tell the user and skip this step.

Run Gemini with the exact following command:

```
agy --dangerously-skip-permissions -p "Run docs/playbook_copyediting.md on this branch."
```

When Gemini is done, review its full diff and make sure that the codebase is formatted, linted, and tests pass. DO NOT EDIT ITS COPY CHANGES except to correct factual errors. If you have to edit copy in order to correct a factual error, re-run the Gemini command afterward.

If format, lint, or tests fail because of Gemini's changes, make the smallest edit that makes them pass, and treat it as a factual fix. Re-run Gemini at most two times. If errors remain after that, tell the user. When checks pass, commit anything uncommitted and push any unpushed commits.
