# Playbook: Copy Editing

Your job is to audit, and maybe replace, every piece of writing changed in this branch: markdown, module docs, doc comments, and code comments. Determine whether the writing:

- follows AST-STE100 guidelines
- uses short words and phrases wherever possible
- communicates at most one idea per sentence
- avoids unnecessary words and repetition
- avoids nonstandard formats like LaTeX
- avoids LLMish patterns

In case of any deviation, replace the whole piece of writing to ensure it compiles with AST-STE100 and the other guidelines above. Replace LaTeX with either a code block or regular text, as appropriate.

Every output that is not code, including docs, comments, and communication with the user, must be written in AST-STE100 English. Convey key ideas in leading sentences to ensure they are easy to find.

Replace long lists of facts and symbols in prose. Such lists are impossible for a human to parse. If you find such a list, replace it with a more appropriate structure. If each individual list item conveys a useful fact to the reader, replace the prose list with a structured list. If the list itself is the meaningful component and its items are just examples, delete the items and relay the concept behind the list.

Replace any table whose longest row is > 100 columns with an alternative that is human-readable.

After the rewrite, all pieces of writing must comply with AST-STE100 guidelines. Structurally, the rewrite must ensure that all writing places key ideas at the top rather than buried somewhere among the factslop.

## Completion

Run in a loop, optionally with sub-agents. Each loop iteration must perform the full task above across the whole branch. Commit after each loop iteration, with a message that future iterations can use . Loop break condition: at the end of each iteration, determine whether more than 5% of the branch's writing changed. If ≤5%, break.

After the loop is complete, make sure that the codebase is formatted, linted, and tests pass. Then check whether a PR exists. If not, create one. If one already exists, rewrite its title + description to match the format of previous commits on main. Write AST-STE100 and focus on conveying concepts concisely rather than listing facts.

## LLMish Examples

A few non-exhaustive examples of idiosyncratic LLM-ish tells:

- "when" instead of "if" for conditionals
- "spells" and "spelling"
- "carry" and "carries"
- "refuse" and "refusal"
- "names" as a verb
- "no <noun>"
- "<noun> resolves". No! The correct form is "<noun> is resolved". Passive objects are acted upon; they do not act.
- "a <noun> nothing <verb>s"
- "Every <noun> is one …"
- Use of "one" instead of "a"
- Vague shorthand
- Colloquialisms
- Unecessary parenthetical clauses
- Britishisms; this is an American codebase
- Description of what isn't rather than description of what is
- Long runs of facts and symbols
- Middle voice

All of the above apply both to regular writing and to symbol names, which may be vague or sloppy. Rename any offending symbols to conform with AST-STE100 guidelines. However, do not rewrite any logic beyond renaming the symbols.
