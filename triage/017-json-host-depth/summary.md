# A JSON document that `json.parse` accepts fails or crashes at the host boundary

TODO item 017 (Embedding): "Convert `Json` values to and from Rust without recursion so…"

**Pain.** My script parses a 300-level JSON document and gives it to my Rust function that takes `Json`. In a release build, the call fails with `limit: value too deeply nested to convert`, although `json.parse` and `json.encode` accept 512 levels. In a debug build on a 512 KiB thread (the iOS default), a 100-level document aborts the whole host process with a stack overflow.

**Why it matters.** The script cannot know which documents the host accepts. An untrusted script can abort a debug host process, and a script cannot catch a stack overflow.

**Reproduce.** Copy `repro.rs` into a scratch crate (see the header comment), then run `json017 <depth> <parse|in|out>`. `in` gives a parsed document to the host. `out` gives a host-built document to the script. The repro runs the engine on a 512 KiB thread.

| build   | depth | `parse` (control) | `in` / `out`                                        |
| ------- | ----- | ----------------- | --------------------------------------------------- |
| release | 256   | ok                | ok                                                  |
| release | 257   | ok                | `Err(Runtime { "limit: value too deeply nested to convert" })` |
| release | 512   | ok                | same error                                          |
| debug   | 80    | ok                | ok                                                  |
| debug   | 100   | ok                | `thread has overflowed its stack` (process abort)   |

**Aspirational.** A host function that takes or returns `Json` accepts every document that `json.parse` accepts (512 levels), on a 512 KiB stack, in debug and release builds.

Note: in a debug build, `json.parse` alone also overflows a 512 KiB stack at 256 levels. That failure is in the parser, not in the conversion.
