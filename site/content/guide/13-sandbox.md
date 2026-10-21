+++
title = "Sandbox and policies"
description = "Configure access to resources with policies and limits."
+++

By default, a Sand script is isolated from the host system: it cannot access files, network sockets, environment variables, or standard input unless the active policy grants permission. The engine checks every external operation against the policy before executing it.

## Capabilities

Each external operation requests a capability from the policy:

| Capability                  | Controlled operations                                    |
| --------------------------- | -------------------------------------------------------- |
| `read`, `write`             | file system access, checked per canonical path           |
| `net`, `http`               | TCP/TLS connections and HTTP requests, by host:port      |
| `listen`                    | TCP listeners, by bind address                           |
| `env`                       | environment variable reads, by variable name             |
| `stdin`, `stdout`, `stderr` | standard input, output, and error streams                |
| `timezone`                  | `time.local_zone()`                                      |
| `import`                    | script module imports, by file path                      |
| `custom`                    | guarded functions in [native modules](/guide/embedding/) |

Sand has neither a foreign function interface nor process-spawning APIs; every host interaction passes through these capabilities.

## Policy decisions

A policy evaluates each capability request to a decision:

- **allow**: the engine performs the operation.
- **deny**: the engine blocks the operation and returns a permission error to the script.
- **prompt**: the engine suspends execution and asks the host prompter (`sand run --prompt` prompts on the terminal). If the host has not configured a prompter, the request is denied.

## Behavior on denial

If the policy denies a file or network operation, the standard library function returns an `Err` with kind `"permission_denied"`. A denied environment variable read returns `None`, and a denied `timezone` request causes `time.local_zone()` to return `UTC`:

```sand
import env
import http

match http.get("https://example.com/") {
    Ok(resp) => print(resp.status)
    Err(err) => print("\(err.kind): \(err.message)")
}
print(env.get("HOME"))
```

Unwrapping a denied `Result` with `?` at the top level terminates the run:

```sand fails
import http

let resp = http.get("https://example.com/")?
print(resp.status)
```

A denied write to `stdout` or `stderr` aborts the run immediately, and a denied `import` fails at compile time.

## Command-line flags

`sand run`, `sand test`, and `sand repl` deny all capabilities except `stdout`, `stderr`, and imports within the module root. Grant capabilities with CLI flags:

```sh
sand run main.sand --allow-read=data --allow-write=out
sand run fetch.sand --allow-net=api.example.com --allow-env=HOME,LANG
sand run main.sand --allow-stdin --max-time=10s
```

| Flag                                | Granted capabilities                         |
| ----------------------------------- | -------------------------------------------- |
| `--allow-read=PATHS`                | read access to the listed files/directories  |
| `--allow-write=PATHS`               | read and write access to the listed paths    |
| `--allow-net=HOSTS`                 | TCP and HTTP access to the listed hosts      |
| `--allow-listen=ADDRS`              | bind access on the listed local addresses    |
| `--allow-env=NAMES`                 | read access to matching env vars (`AWS_*`)   |
| `--allow-stdin`, `--allow-timezone` | standard input and local time zone           |
| `--allow-all`                       | all capabilities (use only for trusted code) |
| `--policy=FILE`                     | rules and resource limits from a policy file |

Separate multiple values with commas. Allowing a directory grants access to all files and subdirectories within it.

## Policy files

A policy file declares capability rules and resource limits in a single file, with a rule per line and `#` comments:

```policy
# Allow read/write in the project directory, except .env files.
allow ./
deny ./**/.env
allow read ~/.gitconfig
allow http GET HEAD api.github.com
allow env HOME LANG LC_*
default prompt
limit memory 64M
limit time 10s
```

Pass the policy file to `sand run`:

```sh
sand run main.sand --policy=app.policy
```

File paths in a policy file must start with `/`, `./` (resolved relative to the policy file directory), `../`, or `~/`. `allow PATH` grants both read and write access; `allow read PATH` grants read-only access. `default` sets the fallback decision (`deny`, `allow`, or `prompt`) for unmatched requests.

`sand check`, `sand lint`, and `sand fmt` also operate on `.policy` files, and `sand lint` warns about shadowed or redundant rules.

### Rule precedence

If multiple rules match a request, the most specific rule takes precedence. In the policy above, `deny ./**/.env` is more specific than `allow ./` for `./data/.env`, so access is denied:

- File paths with more path segments are more specific; ties break toward fewer wildcards.
- Exact hostnames (`api.example.com`) are more specific than wildcard domains (`*.example.com`), which are more specific than `*`. A port constraint increases specificity.
- Exact environment variable names are more specific than prefix patterns.
- If an `allow` rule and a `deny` rule have equal specificity, `deny` wins.

Sand canonicalizes paths and resolves `..` and symbolic links before checking the policy, preventing directory traversal outside allowed trees.

### Private network protection

By default, Sand blocks connections to loopback, link-local, and RFC 1918 private addresses (`localhost`, `127.0.0.1`, `10.0.0.0/8`, and similar ranges) even if `--allow-net=*` is set. To permit private network access, either name the target host explicitly in an allow rule (`--allow-net=localhost:8080`) or pass `--allow-private-net` (`allow private-net` in a policy file).

Sand also checks every IP address returned by DNS resolution so that a public hostname resolving to an internal IP address cannot bypass private network protection.

## Resource limits

Resource limits bound the memory, time, and host handles that a script can consume. If a script exceeds the memory, wall-clock time, CPU time, instruction count, or output limit, Sand aborts execution immediately and `sand run` exits with code 3:

```sh
$ sand run loop.sand --max-time=50ms
sand: time limit exceeded
```

| Limit           | Flag                   | Default   |
| --------------- | ---------------------- | --------- |
| memory          | `--max-memory=SIZE`    | 512 MiB   |
| wall-clock time | `--max-time=DURATION`  | unlimited |
| CPU time        | `--max-cpu=DURATION`   | unlimited |
| instructions    | `--max-instructions=N` | unlimited |

In a policy file, set limits with `limit memory 64M`, `limit time 10s`, `limit cpu 2s`, or `limit instructions 1_000_000`. Additional limits bound output bytes, call stack depth, concurrent task count, open file and connection handles, and HTTP body size. Exceeding the call stack depth or task limit aborts the run with exit code 1. Exceeding the open handle or HTTP body limit returns a recoverable `Err` with kind `"limit"`. For the complete list of limits and policy rules, see [§15](/spec/#15-resource-limits) and [§14.7](/spec/#147-policy-files) of the specification.
