SandLang is a sandboxed bytecode interpreter with the following requirements:

- implemented in 100% safe Rust
- fully sandboxed for all system resources
- hard limits on per-engine memory, CPU, etc.
- no sandbox escape mechanisms: no IPC, subprocesses, mmap, etc.
- single threaded engine
- multiple engines may be safely embedded in a single Rust crate and run in the same process without issue
- maximum performance with no JIT
- type checking, linting, and formatting built in
- syntax that encourages elegant, correct code
- modules and imports
- native Rust modules with type interop
- extensible standard library written in Rust
- runs on every major platform: iOS, macOS, Windows, Linux, Android, Web

## Playbooks

- Follow docs/playbook_feature.md for a feature request.
- Follow docs/playbook_reviewing.md for a system or code review request.
- Follow docs/playbook_copyediting.md for a copy editing request.

If playbook instructions conflict with this doc, follow this doc and communicate the conflict to the user.

## Be Bold

Approach each task from first principles. This is a greenfield codebase with no legacy code to maintain. You own the whole codebase and can make any change, anywhere.

## Doc and Comment Guidelines

The standard library must be documented with module docs. Its canonical user-visible docs are in sandlang.com's stdlib reference. It does not need to be documented in SPEC.md. Doc comments should explain an API for the benefit of the caller. Code comments should be used only to explain unintuitive code.

A symbol's doc comment, and especially its first line, must reflect the symbol's purpose. For example, if a type represents a HTTP route, the type's doc comment should be its HTTP route because its purpose is to serve that route. Avoid using a doc comment to elaborate on implementation details that the caller does not care about. If such implementation details are actually necessary because of non-obvious code, flag it to the user.

Before writing a code comment, ask yourself whether the comment is reflective of poorly written code. Is it an expression of code that is so intrinsically complex that the extra writing is the only way to make the code understandable? Typically, there is a better way to make the code or interface understandable. Exceptions to this rule tend to be interfaces that _do_ have inherent complexity, like a regex library or policy file format.

Avoid changelog-style comments at all cost. They pollute the codebase and harm the reader's understanding of the code as it exists.

## Writing and User Communication Guidelines

Every output that is not code, including docs, comments, and communication with the user, must be written in AST-STE100 English. Convey key ideas in leading sentences to ensure they are easy to find. Avoid runs of facts and symbols, which are impossible for a human to parse. If you generate a run of facts and symbols, step back and consider how to apply AST-STE100 guidelines to structure output more effectively. See the copy editing playbook for detailed guidelines.

Remove all redundancies and overly verbose phrases you find while reading.

Never write with middle voice. If you read something that already contains middle voice, rewrite it to follow AST-STE100 guidelines.

### Teaching

When communicating with a human user, the user may ask a question in a way that suggests they do not understand a concept. When this occurs, it is your job to teach them. The user is more likely to have 5% context than 99% context, and teaching a concept to a person who does not understand it is very different than explaining a concept to a fellow LLM with the same context and knowledge. A human must be taught incrementally. Walk the user through one idea at a time, one sentence at a time, and pause after each to let them think, ask questions, and follow up. One tic to avoid here is a repeated offer to "Let me know when you want to follow up." Say it at most once and then let the user do it themselves.
