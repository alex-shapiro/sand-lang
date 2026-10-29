+++
title = "Sandbox and policies"
description = "Configure access to resources with policies and limits."
+++

Sand isolates each script from the host system and checks every external operation against the sandbox policy before executing it. By default, an embedded engine denies capabilities that do not match an `allow` rule, and `sand run` prompts on the terminal.

## Capabilities

The engine checks a capability for each external operation:

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

Sand does not provide a foreign function interface or process-spawning APIs. Every host interaction passes through these capabilities.

## Policy decisions

A policy evaluates each capability request to a decision:

- **allow**: the engine performs the operation.
- **deny**: the engine blocks the operation and returns a permission error to the script.
- **prompt**: the engine suspends execution and queries the host prompter. In the CLI, `sand run` prompts on the terminal. If the host does not configure a prompter, the engine denies the request. Capabilities that do not match any rule evaluate to `prompt`.

## Behavior on denial

If the policy denies a file or network operation, the standard library function returns an `Err` with kind `"permission_denied"`. A denied environment variable read returns `None`, and `time.local_zone()` returns `UTC` if the `timezone` capability is denied:

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

`sand run`, `sand test`, and `sand repl` allow scripts to write to `stdout` and `stderr`, read standard input and the local time zone, and import modules. For all other capabilities, Sand prompts on the terminal unless the request matches a rule. Pass `--allow`, `--deny`, or `--prompt` with a policy rule body to configure rules on the command line:

```sh
sand run main.sand --allow='read ./data' --allow='write ./out'
sand run fetch.sand --allow='net api.example.com' --allow='env HOME LANG'
sand run server.sand --allow='listen localhost:8080' --deny='net 10.0.0.0/8'
```

| Flag              | Effect                                                                      |
| ----------------- | --------------------------------------------------------------------------- |
| `--allow=RULE`    | allows capabilities matched by `RULE`, e.g. `--allow='read ./data'`         |
| `--deny=RULE`     | denies capabilities matched by `RULE`, e.g. `--deny=stdin`                  |
| `--prompt=RULE`   | prompts for capabilities matched by `RULE`, e.g. `--prompt='write ./'`      |
| `--policy=FILE`   | loads rules and resource limits from `FILE`                                 |
| `--allow-all`     | allows unmatched capabilities for trusted scripts                           |
| `--no-prompt`     | denies unmatched and `prompt` capabilities instead of prompting             |

Quote rules that contain spaces or `*`, and separate multiple arguments in a rule with spaces or commas. Relative paths are resolved from the current working directory. An `allow` rule for a directory grants access to all files and subdirectories inside it. If `--no-prompt` is set or a controlling terminal is unavailable, Sand denies `prompt` decisions and prints the `--allow` flag that grants the capability.

User rules from flags and `--policy` files take precedence over the default CLI rules. For example, `--prompt=stdin` prompts before reading standard input, and `--allow-all --prompt='write **'` allows all operations except file writes, which still prompt. Flags and policy file rules belong to the same rule set, so an `--allow` flag does not override a `deny` rule of equal or higher specificity in a policy file.

## Policy files

A policy file declares capability rules and resource limits, with a single statement per line and `#` comments:

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

File paths in a policy file must start with `/`, `./`, `../`, `~/`, or `**/`. Relative paths are resolved from the directory containing the policy file. `allow PATH` grants both read and write access; `allow read PATH` grants read-only access. Capabilities that do not match any rule evaluate to `prompt`.

`sand check`, `sand lint`, and `sand fmt` also operate on `.policy` files. `sand lint` warns about shadowed or redundant rules.

### Rule precedence

If a request matches multiple rules, Sand applies the most specific rule. In the policy above, `deny ./**/.env` is more specific than `allow ./` for `./data/.env`, so Sand denies access:

- File paths with more path segments are more specific; ties break toward fewer wildcards.
- Exact hostnames such as `api.example.com` are more specific than wildcard domains such as `*.example.com`, which are more specific than `*`. Rules with a port are more specific than rules without a port.
- Exact environment variable names are more specific than prefix patterns.
- If two rules have equal specificity, `deny` wins over `allow`, and `allow` wins over `prompt`.

Sand canonicalizes paths and resolves `..` and symbolic links before checking the policy, preventing directory traversal outside allowed trees.

### Prompt rules

For a `prompt` rule, the engine queries the prompter instead of allowing or denying the request directly. Use `prompt` rules for operations that require confirmation inside an allowed scope, such as writing build output or sending `POST` requests:

```policy
allow ./
prompt write ./dist
prompt http POST api.github.com
```

If the host does not configure a prompter, the engine denies `prompt` decisions.

### Encrypted and plaintext connections

By default, `allow net` and `allow http` rules apply only to TLS connections via `https` and `net.connect_tls`. Specify `plaintext` to allow unencrypted `http` and `net.connect` connections, or `tls plaintext` to allow both. `deny` and `prompt` rules without a transport keyword apply to both TLS and plaintext connections:

```policy
allow http api.github.com            # https:// only
allow http plaintext localhost:8080  # http:// only
allow net tls plaintext db.internal  # both
deny net plaintext *                 # block unencrypted connections
deny net 10.0.0.0/8                  # block both transports
```

Transport keywords do not change rule specificity: `allow net plaintext x.example` and `deny net x.example` have equal specificity, so `deny` wins for both transports. To allow only plaintext connections to a host, pair `allow net plaintext x.example` with `deny net tls x.example`.

### Resolved addresses

After a hostname rule allows a connection, Sand resolves the hostname and checks each returned IP address against the IP and CIDR rules in the policy. This check prevents DNS rebinding and blocks public hostnames that are resolved to denied internal networks. For example, with `allow net tls plaintext *` and `deny net 10.0.0.0/8`, Sand denies connections to any wildcard-matched hostname that is resolved to `10.0.0.5`. An exact hostname rule such as `allow net db.internal` is more specific than a CIDR range and allows connections to `db.internal` at `10.0.0.5`, whereas wildcard rules such as `*` or `*.example.com` do not override CIDR rules.

Sand resolves `localhost` and `*.localhost` directly to `127.0.0.1` and `::1` without querying DNS, per RFC 6761. A `deny` or `prompt` rule for `localhost` also applies to `*.localhost`. Because hostname rules do not apply to IP literals, block local connections completely with `deny net localhost 127.0.0.0/8 ::1`.

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
