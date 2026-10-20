+++
title = "Sandbox and policies"
description = "How Sand checks every effect of a script against a policy, and how to set the policy and resource limits."
+++

A Sand script has no access to the outside world by default. It cannot read files, connect to the network, read environment variables or read standard input until a policy allows it. The engine checks each such effect against the policy before the effect happens.

## Capabilities

Each effect makes a capability request. The policy decides each request. These are the capabilities:

| Capability                  | Requested by                                   |
| --------------------------- | ---------------------------------------------- |
| `read`, `write`             | file system access, for each path              |
| `net`, `http`               | network connections and HTTP requests, by host |
| `listen`                    | sockets that accept connections                |
| `env`                       | environment variables, by name                 |
| `stdin`, `stdout`, `stderr` | the standard streams                           |
| `timezone`                  | `time.local_zone()`                            |
| `import`                    | script modules, by path                        |
| `custom`                    | functions of [host modules](/guide/embedding/) |

The script has no other path to the system. Sand has no foreign function interface, and it does not start processes.

## Decisions

The policy gives one of three decisions for each request:

- **allow**: the effect happens.
- **deny**: the effect does not happen, and the function returns an error.
- **prompt**: the engine asks the host, and no task runs until the host answers. On the command line, `sand run --prompt` asks you on the terminal. If no one can answer, Sand denies the request.

## What a script sees

A denied I/O function returns an `Err` of kind `permission_denied`. The script can handle it like any other error. A denied environment variable looks like an unset variable.

```sand
import env
import http

match http.get("https://example.com/") {
    Ok(resp) => print(resp.status)
    Err(err) => print("\(err.kind): \(err.message)")
}
print(env.get("HOME"))
```

If the script uses `?` at the top level, the denial stops the run:

```sand fails
import http

let resp = http.get("https://example.com/")?
print(resp.status)
```

A denied `print` stops the run. A denied `import` is a compile error.

## Command-line flags

`sand run`, `sand test` and `sand repl` deny everything except output unless you allow it. Use flags to allow access:

```sh
sand run main.sand --allow-read=data --allow-write=out
sand run fetch.sand --allow-net=api.example.com --allow-env=HOME,LANG
sand run main.sand --allow-stdin --max-time=10s
```

| Flag                                | Allows                                       |
| ----------------------------------- | -------------------------------------------- |
| `--allow-read=PATHS`                | reading these files and directories          |
| `--allow-write=PATHS`               | reading and writing them                     |
| `--allow-net=HOSTS`                 | connections and HTTP requests to these hosts |
| `--allow-listen=ADDRS`              | listening on these addresses                 |
| `--allow-env=NAMES`                 | these environment variables (`AWS_*`, `*`)   |
| `--allow-stdin`, `--allow-timezone` | standard input, the local time zone          |
| `--allow-all`                       | everything (use only for trusted scripts)    |
| `--policy=FILE`                     | the rules and limits of a policy file        |

Separate values with commas. A directory rule also covers all files and directories in it.

## Policy files

A policy file keeps the rules and limits of a program in one place. Each line is one rule. A `#` starts a comment.

```policy
# Read and write the project, but keep secrets out.
allow ./
deny ./**/.env
allow read ~/.gitconfig
allow http GET HEAD api.github.com
allow env HOME LANG LC_*
default prompt
limit memory 64M
limit time 10s
```

Run a script with it:

```sh
sand run main.sand --policy=app.policy
```

Each path must start with `/`, `./` (the directory of the policy file), `../` or `~/`. `allow PATH` allows reading and writing; `allow read PATH` allows only reading. `default` sets the decision for requests that no rule matches. Without it, Sand denies them.

`sand check`, `sand lint` and `sand fmt` also work on policy files. `sand lint` reports rules that have no effect.

### Which rule wins

When more than one rule matches a request, the most specific rule wins. In the file above, `deny ./**/.env` is more specific than `allow ./` for `./data/.env`, so Sand denies access to that file:

- A path with more segments is more specific. If two paths have the same number of segments, the path with fewer wildcards wins.
- An exact host is more specific than `*.example.com`, which is more specific than `*`. A port makes a rule more specific.
- An exact environment variable name is more specific than a pattern.
- If an allow rule and a deny rule are equally specific, the deny rule wins.

Sand resolves `..` and symbolic links before it checks a path, so a script cannot use them to leave an allowed directory.

### Private networks

Sand denies connections to private and internal addresses, such as `localhost`, `127.0.0.1` and `10.0.0.0/8`. `--allow-net=*` does not allow them. To allow one, name it in a rule (`--allow-net=localhost:8080`) or use `--allow-private-net`. In a policy file, write `allow private-net`.

Sand also checks each address that a host name resolves to. Thus a public host name that resolves to a private address cannot reach your internal network.

## Resource limits

Limits stop a script that uses too much memory or time. When a script goes over a limit, the run stops at once. The script cannot catch this error, and `sand run` exits with code 3. For example, for a script with an endless loop:

```sh
$ sand run loop.sand --max-time=50ms
sand: time limit exceeded
```

The most important limits are these:

| Limit           | Flag                   | Default   |
| --------------- | ---------------------- | --------- |
| memory          | `--max-memory=SIZE`    | 512 MiB   |
| wall-clock time | `--max-time=DURATION`  | unlimited |
| CPU time        | `--max-cpu=DURATION`   | unlimited |
| instructions    | `--max-instructions=N` | unlimited |

In a policy file, write `limit memory 64M`, `limit time 10s`, `limit cpu 2s` or `limit instructions 1_000_000`. Other limits control the output size, the number of tasks, the number of open files and connections, and the size of HTTP bodies. [§15 of the specification](https://github.com/alex-shapiro/sand-lang/blob/main/docs/SPEC.md#15-resource-limits) lists every limit and its default, and [§14.7](https://github.com/alex-shapiro/sand-lang/blob/main/docs/SPEC.md#147-policy-files) every policy rule.
