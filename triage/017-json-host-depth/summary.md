# Recursive `Json` conversion at the host boundary fails or overflows the stack

`json.parse` and `json.encode` accept up to 512 nesting levels, but converting
a `Json` value across the Rust host boundary fails at 257 levels in release
builds and overflows a 512 KiB thread stack at 100 levels in debug builds. An
untrusted script can abort a debug host process by passing a deeply nested
`Json` value to a native function.

Build and run `repro.rs` on a 512 KiB thread with `<depth> <parse|in|out>`:

| Build   | Depth | `parse` | `in` / `out`                                                  |
| ------- | ----- | ------- | ------------------------------------------------------------- |
| release | 256   | ok      | ok                                                            |
| release | 257   | ok      | `Err(Runtime { "limit: value too deeply nested to convert" })` |
| debug   | 80    | ok      | ok                                                            |
| debug   | 100   | ok      | stack overflow                                                |

`Json` conversion between Sand and Rust should run iteratively and accept all
512 levels on a 512 KiB stack in both debug and release builds.
