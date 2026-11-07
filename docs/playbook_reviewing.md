# Playbook: Reviewing a Branch

Review this branch's diff against main and issue fixes on this branch in order to converge it to PR-worthiness. Each iteration must audit the full branch diff. Focus on product impact and system design.

## Audit Criteria

### Product Impact

1. How does this change materially improve or degrade the user-facing product? More useful featureset, more usable interface, higher reliability, improved safety, faster performance, etc.

2. After this change, do all impacted features work as a normal user would expect?

### System Design

3. Typically, we want each user action to follow a single path through code. Where does this branch introduce parallel paths, creating distinct ways to cover minor variants of doing basically one thing? Inventory, then explore: what alternative designs avoid or minimize splitting, or even reverse an existing split where it exists by merging two pathways together?

4. If two disparate pieces of code must stay in sync, it is preferable to enforce that dependency statically (shared types, macros, etc.) than to enforce at runtime. Look for, and inventory, such dependencies in new and existing code. Then explore: what alternative designs would remove the dependencies, unify them, or sync them statically?

5. System types and symbols, and code in general should be understandable. Look for, and inventory, places in code where type signatures are less intelligible: heavy nesting, huge tuples, signatures with lots of parameters. What are the upstream (or downstream) requirements that push for such a complex type signature? Is there a way to simplify those requirements and thus the type signature?

6. Typically, we want the system to meet product goals and priorities while avoiding diverging too far from industry best practices. A divergence may be justified, and justification is a judgment call: the larger the divergence, the more important to upholding system goals the justification must be. Where are the largest divergences from best practice in this branch, and where are the justifications most suspect?

7. Typically, we want the system to be as small as possible while still meeting product goals and priorities. Each LoC requires understanding and maintenance. What can we cut?

## Fixing

An audit may reveal problems that can be fixed or improved with limited or no product tradeoffs. For each such problem, verify that the problem is legitimate, and that fixing will meaningfully improve the codebase in at least one of the audit criteria above without meaningfully regressing the others.

Fix any such problem automatically and, where applicable, ensure that tests prove the soundness of the new approach. Each fix MUST adhere to the audit criteria.

Commit each fix independently, with a concise description of the problem and the solution. Include any tests, experiments, measurements, and outcomes so that follow-up work can reference past findings easily.

## Communicating Tradeoffs

If an audit finds a design problem where the best solution requires a significant product trade-off, write down the problem, potential solutions, and tradeoffs concisely in a standalone, short (short!), readable section of a docs/TRADEOFFS.md file. This doc will allow follow-up work to reference branch-specific, unresolved tradeoffs. After a tradeoff is resolved, remove it from the doc and write the decision and its reason in a commit message. A future auditor can read the commit message to understand the tradeoff decision.

Merely documenting a design issue is less desirable than fixing it. If a tradeoff is weak or nonexistent, prefer to fix a design issue than to note it. But if you find that there is a significant tradeoff that would force changes to the nature of the product, it is better to note it in TRADEOFFS.md than to commando-fix it prior to my input. Breaking backwards compatibility is ok; a tradeoff must be a _real_ product or systemic tradeoff.

## Review Loop

When you are done with the audit, fixes, and notes, kick off another independent review cycle, again auditing the full branch diff. Each iteration must have access to past fixes and tradeoff notes. Break the review loop if an audit iteration finds no significant new flaws that have not already been fixed or noted. If the loop continues past five iterations, break the loop and report remaining flaws to the user.

At the end of the review, report unresolved tradeoffs to the user. Move each unresolved tradeoff to TODO.md and delete TRADEOFFS.md.

Approach each review iteration and each fix from first principles; we control all repos and can make any changes necessary. If a suboptimal approach is being taken and you are justified in replacing it with a better alternative, do so.

Use sub-agents as necessary, especially to keep reviewers independent from fixers. If you use a sub-agent, ensure that it has sufficient context across restarts. Context: in the past, sub-agents have been killed and restarted with half context, resulting in suboptimal design and implementation.

Start each reviewer sub-agent with the exact prompt:

```
Run the audit portion of a review iteration, following `docs/playbook_reviewing.md`.
Review the full diff in this branch, up to its most recent commit, against origin/main.
Read AGENTS.md and, if it exists, docs/TRADEOFFS.md.

Do not edit files, commit, or push. For each finding, give file:line, the
problem, a proposed fix, and whether the fix requires a real product tradeoff.
Rank findings from most to least significant, and label each as "significant" or "minor".
```

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
