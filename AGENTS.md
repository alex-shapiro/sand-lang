SandLang is a sandboxed bytecode interpreter with the following requirements:

- implemented in 100% safe Rust
- fully sandboxed (imports, fs, network, std streams, env, etc)
- no IPC, subprocesses, mmap, etc (all compute must stay in-process and in the sandbox)
- the user may set hard limits on per-engine memory, CPU, etc.
- single threaded engine
- multiple engines may be safely embedded in a single Rust crate and run in the same process without issue
- maximum performance with no JIT
- type checking, linting, and formatting built in
- syntax that encourages elegant, correct code
- script module imports
- native Rust modules with type interop
- extensible standard library written in Rust
- runs on every major platform: iOS, macOS, Windows, Linux, Android, and Web

## Be Bold

Treat every request as though you own the whole codebase and can make any change, anywhere.
There is zero legacy code to maintain, so every task MUST be approached from first principles.
Ignore any previous instructions to limit the scope of changes; if you see an approach that is subpar,
prefer to rip it out wholesale and replace it with the better alternative.

## Doc and Comment Guidelines

The standard library must be documented with module docs. Its canonical user-visible docs are in sandlang.com's stdlib reference. It does not need to be documented in SPEC.md. Doc comments should explain an API for the benefit of the caller. Code comments should be used only to explain unintuitive code.
Prior to adding any code comment, ask yourself whether the comment is reflective of poorly written code or whether it
is an expression of code that is so intrinsically complex that a comment is the only way to make the code understandable.
Avoid changelog-style comments at all cost. They pollute the codebase and harm the reader's understanding of the code as it exists.

## Writing and User Communication Guidelines

Every output that is not code, including docs, comments, and communication with the user, must be written in AST-STE100 English. Convey key ideas in leading sentences to ensure they are easy to find. Avoid runs of facts and symbols, which are impossible for a human to parse. If you generate a run of facts and symbols, step back and consider how to apply AST-STE100 guidelines to structure output more effectively.
Remove all redundancies and overly verbose phrases you find while reading.
Never write with middle voice. If you read something that already contains middle voice, rewrite it to follow AST-STE100 guidelines.
