# Playbook: Reviewing a Branch

Review this branch's diff against main and issue fixes on this branch in order to converge it to PR-worthiness. Each iteration must review the full branch diff. Focus on system design:

- Typically, we want each request to follow one path through code. Where does this branch introduce parallel paths through code, creating more execution pathways to capture minor variants of doing basically one thing? Inventory, then explore: what alternative designs avoid or minimize splitting, or even reverse an existing split where it exists by merging two pathways together?

- If two disparate pieces of code must stay in sync, it is preferable to enforce that dependency statically (shared types, macros, etc.) than to enforce at runtime. Look for, and inventory, such dependencies in new and existing code. Then explore: what alternative designs would remove the dependencies, unify them, or sync them statically?

- System types and symbols should be understandable. Look for, and inventory, places in code where type signatures are less intelligible: heavy nesting, huge tuples, signatures with lots of parameters. What are the upstream (or downstream) requirements that push for such a complex type signature? Is there a way to simplify those requirements and thus the type signature?

- Typically, we want the system to meet product goals and priorities while avoiding diverging too far from industry best practices. A divergence may be justified, and justification is a judgment call: the larger the divergence, the more important to upholding system goals the justification must be. Where are the largest divergences from best practice in this branch, and where are the justifications most suspect?

Where you find design problems that can be fixed or improved with limited or no product tradeoffs, fix the problem automatically and, where applicable, ensure that tests prove the soundness of the updated approach. Commit each fix independently, with a concise description of the problem and the solution. Include any tests, experiments, measurements, and outcomes so that follow-up work can reference past findings easily.

Where you find design problems where the best solution requires a significant product trade-off, write down the problem, potential solutions, and tradeoffs concisely in a standalone, short (short!), readable section of a docs/TRADEOFFS.md file. This doc will allow follow-up work to reference past findings.

Merely documenting a design issue is less desirable than fixing it. If a tradeoff is weak or nonexistent, prefer to fix a design issue than to note it. But if you find that there is a significant tradeoff that would force changes to the nature of the product, it is better to note it than to commando-fix it prior to my input. Breaking backwards compatibility is ok; a tradeoff must be a _real_ product or systemic tradeoff.

Commit and push each fix in its own commit. A commit should contain a brief (brief!) description, as well as evidence of impact. Include references to tests, experiments, or measurements and their outcomes, so follow-up work can reference past findings easily.

When you are done with all fixes, reviews, and notes, kick off another independent review cycle, again reviewing the full branch diff. Each iteration must have access to past fixes and tradeoff notes. Break the review loop if a review iteration finds no significant new design flaws that have not already been fixed or noted. After five iterations, break the loop and report the remaining flaws to the user.

Approach each review iteration and fix from first principles; we control all repos and can make any changes necessary. If a suboptimal approach is being taken and you are justified in replacing it with a better alternative, do so.

Use sub-agents as necessary, especially to keep reviewers independent from fixers. If you use sub-agents, ensure that each sub-agent has sufficient context across restarts. Context: in the past, sub-agents have been killed and restarted with half context, resulting in suboptimal design and implementation.

Run autonomously until the task is complete.

## Copy Editing

The best thinking and coding model is Claude Opus 5.5; however, it is also the worst writer. The model that performs best at writing is Gemini Argon 4. Thus, after the review is complete, you should attempt to call Gemini for copy editing.

Check whether the environment contains the antigravity CLI `agy`. The CLI is available locally but not in a cloud-hosted sandbox. If `agy` is not available, tell the user and skip this step.

Run Gemini with `agy --dangerously-skip-permissions` so that it can work autonomously. Tell it to implement [playbook_copyediting.md](playbook_copyediting.md) on the branch. When Gemini is done, review its full diff and make sure that the codebase is formatted, linted, and tests pass.
