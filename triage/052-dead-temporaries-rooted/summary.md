# Dead temporary registers stay rooted across native calls and cause premature out-of-memory errors

Inlined temporary allocations stay in caller frame registers across subsequent
native calls, which prevents the garbage collector from reclaiming dead values.
In `repro.sand` and `loop.sand`, each iteration allocates a temporary 12 MB
string and discards it after `.len()`, but the second iteration fails under a
20 MB memory limit because the first 12 MB string is still rooted.

Run:

```
$ sand run repro.sand --policy=repro.policy --no-prompt
12000000
sand: memory limit exceeded

$ sand run loop.sand --policy=repro.policy --no-prompt
12000000
sand: memory limit exceeded
```

Clearing dead temporary registers before native calls allows the garbage
collector to reclaim unreachable temporaries so that both scripts complete
within `limit memory 20M`.
