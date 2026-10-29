+++
title = "Sandbox and policies"
description = "Configure access to resources with policies and limits."
+++

A Sand script is isolated from the host system: the engine checks every external operation (files, network sockets, environment variables, standard input) against the policy before it executes the operation. By default, an embedding host denies what no rule allows, and `sand run` asks you.

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
| `custom`                    | custom capabilities checked by [native modules](/guide/embedding/) |

Sand has neither a foreign function interface nor process-spawning APIs. Every host interaction passes through these capabilities.

## Policy decisions

A policy evaluates each capability request to a decision:

- **allow**: the engine performs the operation.
- **deny**: the engine blocks the operation and returns a permission error to the script.
- **prompt**: the engine suspends execution and asks the host prompter (`sand run` asks on the terminal). If the host has not configured a prompter, the engine denies the request. A request that no rule matches gets this decision.

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

`sand run`, `sand test`, and `sand repl` let scripts write to `stdout` and `stderr`, read standard input and the local time zone, and import modules. For anything else, Sand asks you on the terminal. Rules decide requests ahead of time: `--allow`, `--deny`, and `--prompt` each take one policy file rule without its verb:

```sh
sand run main.sand --allow='read ./data' --allow='write ./out'
sand run fetch.sand --allow='net api.example.com' --allow='env HOME LANG'
sand run server.sand --allow='listen localhost:8080' --deny='net 10.0.0.0/8'
```

| Flag              | Effect                                                      |
| ----------------- | ----------------------------------------------------------- |
| `--allow=RULE`    | allows what the rule matches, e.g. `--allow='read ./data'` |
| `--deny=RULE`     | denies what the rule matches, e.g. `--deny=stdin`           |
| `--prompt=RULE`   | asks about what the rule matches, e.g. `--prompt='write ./'` |
| `--policy=FILE`   | rules and resource limits from a policy file                |
| `--allow-all`     | allows what none of your rules match (only for trusted code) |
| `--no-prompt`     | denies instead of asking                                    |

Quote rules that contain spaces or `*`. Separate several values of one rule with spaces or commas. Relative paths start from the working directory. Allowing a directory grants access to all files and subdirectories within it. Without a terminal, or with `--no-prompt`, Sand denies what it would ask about and shows the `--allow` option that allows it.

The default rules decide only what none of your rules match: `--prompt=stdin` asks before the script reads standard input, and `--allow-all --prompt='write **'` allows everything but asks before each write. The flags and the rules of a policy file are one set of rules, so a flag cannot loosen a `deny` rule of the file for the same pattern.

## Policy files

A policy file declares capability rules and resource limits in a single file, with a rule per line and `#` comments:

```policy
# Allow read/write in the project directory, except .env files.
allow ./
deny ./**/.env
allow read ~/.gitconfig
allow http GET HEAD api.github.com
allow env HOME LANG LC_*
limit memory 64M
limit time 10s
```

Pass the policy file to `sand run`:

```sh
sand run main.sand --policy=app.policy
```

File paths in a policy file must start with `/`, `./` (resolved relative to the policy file directory), `../`, `~/`, or `**/` (any directory). `allow PATH` grants both read and write access; `allow read PATH` grants read-only access. A request that no rule matches goes to the prompter: `sand run` asks you, and a host without a prompter denies it.

`sand check`, `sand lint`, and `sand fmt` also operate on `.policy` files. `sand lint` warns about shadowed or redundant rules.

### Rule precedence

If multiple rules match a request, the most specific rule takes precedence. In the policy above, `deny ./**/.env` is more specific than `allow ./` for `./data/.env`, so Sand denies access:

- File paths with more path segments are more specific; ties break toward fewer wildcards.
- Exact hostnames (`api.example.com`) are more specific than wildcard domains (`*.example.com`), which are more specific than `*`. A port constraint increases specificity.
- Exact environment variable names are more specific than prefix patterns.
- If two rules have equal specificity, `deny` wins over `allow`, and `allow` wins over `prompt`.

Sand canonicalizes paths and resolves `..` and symbolic links before checking the policy, preventing directory traversal outside allowed trees.

### Prompt rules

A `prompt` rule asks the user instead of deciding. Use it for operations that need a human decision each time, such as writing build output or posting to an API:

```policy
allow ./
prompt write ./dist
prompt http POST api.github.com
```

Without a prompter, a `prompt` rule denies the request.

### Encrypted and plaintext connections

An `allow net` or `allow http` rule matches TLS connections (`https`, `net.connect_tls`) unless it names a transport. Write `plaintext` for unencrypted connections (`http`, `net.connect`), or `tls plaintext` for both. A `deny` or `prompt` rule without a transport matches both, so it cannot miss the unencrypted connections:

```policy
allow http api.github.com            # https:// only
allow http plaintext localhost:8080  # http:// only
allow net tls plaintext db.internal  # both
deny net plaintext *                 # no unencrypted connections
deny net 10.0.0.0/8                  # neither
```

A transport does not make a rule more specific: `allow net plaintext x.example` and `deny net x.example` deny both. To allow only plaintext, write `deny net tls x.example`.

### Resolved addresses

`localhost` and every name under it (`app.localhost`) are the local machine: Sand connects them to `127.0.0.1` or `::1` without asking DNS. So `deny net localhost` also denies `app.localhost`. Like every rule for a name, it does not match IP addresses: to keep scripts off the local machine, write `deny net localhost 127.0.0.0/8 ::1`.

Sand checks every IP address that DNS returns against the address rules, so that a public hostname cannot lead a script to an address the policy denies. With `allow net tls plaintext *` and `deny net 10.0.0.0/8`, a name that resolves to `10.0.0.5` cannot connect. A rule that names the exact host, such as `allow net db.internal`, is more specific than the range and lets that name connect. A wildcard such as `*` or `*.example.com` does not.

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
