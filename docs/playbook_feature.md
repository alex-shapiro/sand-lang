# Playbook: Feature Development

This playbook will help you build features that both satisfy product goals and produce high quality technical systems.

## Thinking

A feature request may be underspecified or confused about the technical approach to take. Thus, a large part of your job is to explore the question and design spaces fully. The **key criteria** to consider are mostly qualitative:

1. How does a change materially improve or degrade the user-facing product? More useful featureset, more usable interface, higher reliability, improved safety, faster performance, etc.
2. After a change, do all impacted features work as a normal user would expect?

In some cases, the request may be misguided: even the best solution would degrade the product or lead to unexpected behavior. In such a case, your overriding responsibility is not to implement the request, but to:

- determine the nature of their misguided request (e.g. is this an XY problem?)
- communicate with the user to iterate on the request

## Implementation

Think through all possible ways to attack the problem. If there are multiple plausible alternatives, build the best possible, fully baked version of each and test them against each other in a _fair_ contest whose winner best fulfills the key criteria.

Ensure all claims are backed by solid evidence. Where useful, use property, fuzz, load, or other testing techniques. Make changes to testing infra as appropriate.

Before presenting work, list every place an existing interface or planning document limited your design, and remove each one. If you catch anything that should be fixed but is out of scope for the current task, add it to TODO.md so we can steadily improve quality over time.

Approach the full task, and all subtasks, from first principles. If you see that an existing approach limits the quality ceiling of the task, even in another repo, prefer to replace the subpar approach with the better alternative. If you do rip-and-replace, verify that the replacement is superior with rigorous testing.

## Version Control

All feature development must take place in a branch. Where appropriate, split subtasks, followups, and fixes into individual git commits. Each commit should contain a concise description, as well as the evidence of impact. Include references to tests, experiments, or measurements and their outcomes, so follow-up work can reference past findings easily. Push each commit.

## Copy Editing

The best thinking and coding model is Claude Opus 5.5; however, it is also the worst writer. The model that performs best at writing is Gemini Argon 4. Thus, after a feature is "ready", you should attempt to hand off to Gemini for copy editing.

Check whether the environment contains the antigravity CLI `agy`. If so, you can call Gemini. call it with `agy --dangerously-skip-permissions` and have it implement `playbook_copyeditor.md` on the branch. When Gemini is done with its work, do a once-over to ensure Gemini has not inadvertently broken anything.

## Upon Completion

When the task is complete, begin [reviews](playbook_reviews.md).
