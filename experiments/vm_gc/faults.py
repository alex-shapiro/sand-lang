"""Plants the same host-code mistakes in each arm and reports what catches them.

Usage: python3 faults.py

Each fault edits one copy of the crate. The script then builds the copy and
runs that arm's tests. A fault ends as "compile error", "test failure", or
"not detected", once with every test and once without the GC stress test.
Arm 2 faults come in two forms: the natural code, which binds
the handle to a scope, and the form an author writes to silence the compiler,
which calls `.unbind()`.
"""

import os
import pathlib
import shutil
import subprocess
import tempfile

ARM1_HOST = "src/arm1/host.rs"
ARM2_HOST = "src/arm2/host.rs"
ARM3_HOST = "src/arm3/host.rs"

FAULTS = [
    # Arm 1: forgotten roots.
    ("arm1", "list.map: output list not rooted", [
        (ARM1_HOST, "        let out = vm.push_root(out)?;\n", ""),
        (ARM1_HOST, "            vm.list_push(vm.root(out), mapped)?;", "            vm.list_push(out, mapped)?;"),
        (ARM1_HOST, "        Ok(vm.root(out))", "        Ok(out)"),
    ]),
    ("arm1", "list.map: mapped value not rooted", [
        (ARM1_HOST, "            let mark = vm.push_root(mapped)?;\n", ""),
        (ARM1_HOST, "            vm.truncate_roots(mark);\n", ""),
    ]),
    ("arm1", "async native: held user not rooted", [
        (ARM1_HOST, "        let user = vm.async_root(user)?;\n", ""),
        (ARM1_HOST, "let (user, held) = (vm.take_async_root(user), vm.take_async_root(held));", "let held = vm.take_async_root(held);"),
    ]),
    ("arm1", "encoder: walked list not rooted", [
        ("src/arm1/codec.rs", "    let mark = vm.push_root(v)?;\n    let n = vm.list_len(v)?;", "    let mark = vm.root_mark();\n    let n = vm.list_len(v)?;"),
    ]),
    ("arm1", "decoder: items kept in a local vector", [
        ("src/arm1/codec.rs",
         "    let mark = vm.root_mark();\n    while cx.reader.next_item(src!(vm, cx), &mut cx.pos)? {\n        let v = decode_value(vm, cx, p, item)?;\n        vm.push_root(v)?;\n    }\n",
         "    let mark = vm.root_mark();\n    let mut items = Vec::new();\n    while cx.reader.next_item(src!(vm, cx), &mut cx.pos)? {\n        items.push(decode_value(vm, cx, p, item)?);\n    }\n    for v in items {\n        vm.push_root(v)?;\n    }\n"),
    ]),
    ("arm1", "host object: trace skips its callback", [
        (ARM1_HOST, "        visit(self.callback);", "        let _ = visit;"),
    ]),
    # Arm 2: the same mistakes, in the natural bound form.
    ("arm2", "list.map: output list not rooted", [
        (ARM2_HOST, "        let out = vm.new_list(n, gc.reborrow())?.unbind().scope(vm, gc.reborrow())?;", "        let out = vm.new_list(n, gc.reborrow())?;"),
        (ARM2_HOST, "(out.get(gc.nogc()).unbind(), mapped.get(gc.nogc()).unbind())", "(out.unbind(), mapped.get(gc.nogc()).unbind())"),
        (ARM2_HOST, "        Ok(out.get(gc.into_nogc()))", "        Ok(out)"),
    ]),
    ("arm2", "list.map: mapped value not rooted", [
        (ARM2_HOST, "            let mapped = vm.call(callee, &[item], gc.reborrow())?.unbind().scope(vm, gc.reborrow())?;", "            let mapped = vm.call(callee, &[item], gc.reborrow())?;"),
        (ARM2_HOST, "mapped.get(gc.nogc()).unbind())", "mapped.unbind())"),
    ]),
    ("arm2", "async native: held user not rooted", [
        (ARM2_HOST, "        let user = vm.call_function(make_user, &[Value::int(i)], gc.reborrow())?.unbind();\n        let user = vm.async_root(user, gc.reborrow())?;\n", "        let user = vm.call_function(make_user, &[Value::int(i)], gc.reborrow())?;\n"),
        (ARM2_HOST, "let (user, held) = (vm.take_async_root(user, gc.nogc()), vm.take_async_root(held, gc.nogc()));", "let held = vm.take_async_root(held, gc.nogc());"),
    ]),
    ("arm2", "encoder: walked list not rooted", [
        ("src/arm2/codec.rs", "    let v = v.scope(vm, gc.reborrow())?;\n    let n = vm.list_len(v.get(gc.nogc()), gc.nogc())?;", "    let n = vm.list_len(v, gc.nogc())?;"),
        ("src/arm2/codec.rs", "    while let Some(item) = vm.list_get(v.get(gc.nogc()), i, gc.nogc())? {", "    while let Some(item) = vm.list_get(v, i, gc.nogc())? {"),
    ]),
    ("arm2", "decoder: items kept in a local vector", [
        ("src/arm2/codec.rs",
         "        let v = decode_value(vm, cx, p, item, gc.reborrow())?.unbind();\n        vm.push_root(v, gc.reborrow())?;\n    }\n    vm.new_list_from_roots(mark, gc)",
         "        items.push(decode_value(vm, cx, p, item, gc.reborrow())?);\n    }\n    for v in items {\n        vm.push_root(v.unbind(), gc.reborrow())?;\n    }\n    vm.new_list_from_roots(mark, gc)"),
        ("src/arm2/codec.rs", "    let mark = vm.root_mark();\n    while cx.reader.next_item(src!(vm, cx, gc), &mut cx.pos)? {", "    let mark = vm.root_mark();\n    let mut items = Vec::new();\n    while cx.reader.next_item(src!(vm, cx, gc), &mut cx.pos)? {"),
    ]),
    ("arm2", "host object: trace skips its callback", [
        (ARM2_HOST, "        self.callback.trace(visit);", "        let _ = visit;"),
    ]),
    # Arm 2: the same mistakes after `.unbind()` silences the compiler.
    ("arm2 unbind", "list.map: output list not rooted", [
        (ARM2_HOST, "        let out = vm.new_list(n, gc.reborrow())?.unbind().scope(vm, gc.reborrow())?;", "        let out = vm.new_list(n, gc.reborrow())?.unbind();"),
        (ARM2_HOST, "(out.get(gc.nogc()).unbind(), mapped.get(gc.nogc()).unbind())", "(out, mapped.get(gc.nogc()).unbind())"),
        (ARM2_HOST, "        Ok(out.get(gc.into_nogc()))", "        Ok(out.bind(gc.into_nogc()))"),
    ]),
    ("arm2 unbind", "list.map: mapped value not rooted", [
        (ARM2_HOST, "            let mapped = vm.call(callee, &[item], gc.reborrow())?.unbind().scope(vm, gc.reborrow())?;", "            let mapped = vm.call(callee, &[item], gc.reborrow())?.unbind();"),
        (ARM2_HOST, "mapped.get(gc.nogc()).unbind())", "mapped)"),
    ]),
    ("arm2 unbind", "async native: held user not rooted", [
        (ARM2_HOST, "        let user = vm.async_root(user, gc.reborrow())?;\n", ""),
        (ARM2_HOST, "let (user, held) = (vm.take_async_root(user, gc.nogc()), vm.take_async_root(held, gc.nogc()));", "let held = vm.take_async_root(held, gc.nogc());"),
    ]),
    ("arm2 unbind", "decoder: items kept in a local vector", [
        ("src/arm2/codec.rs",
         "        let v = decode_value(vm, cx, p, item, gc.reborrow())?.unbind();\n        vm.push_root(v, gc.reborrow())?;\n    }\n    vm.new_list_from_roots(mark, gc)",
         "        items.push(decode_value(vm, cx, p, item, gc.reborrow())?.unbind());\n    }\n    for v in items {\n        vm.push_root(v, gc.reborrow())?;\n    }\n    vm.new_list_from_roots(mark, gc)"),
        ("src/arm2/codec.rs", "    let mark = vm.root_mark();\n    while cx.reader.next_item(src!(vm, cx, gc), &mut cx.pos)? {", "    let mark = vm.root_mark();\n    let mut items = Vec::new();\n    while cx.reader.next_item(src!(vm, cx, gc), &mut cx.pos)? {"),
    ]),
    # Arm 3: locks held across callbacks and a missing host hook.
    ("arm3", "list.map: lock held across the callback", [
        (ARM3_HOST, "        loop {\n            // Release the lock before the callback runs, so that it can use\n            // the list. A `while let` would hold the lock through its body.\n            let Some(item) = items.lock()?.0.get(i).cloned() else { break };\n",
         "        while let Some(item) = items.lock()?.0.get(i).cloned() {\n"),
    ]),
    ("arm3", "host.fire: lock held across the callback", [
        (ARM3_HOST, "        // The lock must be released before the callback runs.\n        let callback = with_host(vm.arg(args, 0), |host| {\n            host.calls += 1;\n            host.callback.clone()\n        })?;\n        vm.call(&callback, &[x])",
         "        let Value::Host(cell) = vm.arg(args, 0).clone() else { return Err(type_error(\"host\")) };\n        let mut guard = cell.lock()?;\n        let host = (&mut *guard.0 as &mut dyn Any).downcast_mut::<CallbackHost>().ok_or_else(|| type_error(\"host\"))?;\n        host.calls += 1;\n        let callback = host.callback.clone();\n        let result = vm.call(&callback, &[x]);\n        drop(guard);\n        result"),
    ]),
    ("arm3", "encoder: list lock held across items", [
        ("src/arm3/codec.rs", "    let mut i = 0;\n    loop {\n        let Some(item) = l.lock()?.0.get(i).cloned() else { break };\n        encode_value(vm, cx, p, &item)?;\n        i += 1;\n    }\n    check_count(i, n)?;\n    write(vm, cx, Event::CloseList)",
         "    let items = l.lock()?;\n    for item in items.0.iter() {\n        encode_value(vm, cx, p, item)?;\n    }\n    drop(items);\n    let _ = i;\n    write(vm, cx, Event::CloseList)"),
    ]),
    ("arm3", "host object: hook skips its callback", [
        (ARM3_HOST, "        f(&mut self.callback);", "        let _ = f;"),
    ]),
]


def run(cmd, cwd, target):
    return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, env={**os.environ, "CARGO_TARGET_DIR": target})


def main():
    root = pathlib.Path(__file__).parent
    with tempfile.TemporaryDirectory() as tmp:
        copy = pathlib.Path(tmp) / "vm_gc"
        target = str(root / "target" / "faults")
        shutil.copytree(root, copy, ignore=shutil.ignore_patterns("target"))
        print(f"{'arm':<12} {'fault':<44} {'all tests':<14} without gc_stress")
        for arm, name, edits in FAULTS:
            originals = {}
            for path, old, new in edits:
                file = copy / path
                originals.setdefault(path, file.read_text())
                text = file.read_text()
                assert old in text, f"{arm}: {name}: edit not found in {path}"
                file.write_text(text.replace(old, new, 1))
            test = arm.split()[0]
            if run(["cargo", "build", "--tests", "--quiet"], copy, target).returncode != 0:
                results = ["compile error"] * 2
            else:
                results = []
                for skip in [[], ["--", "--skip", "gc_stress"]]:
                    failed = run(["cargo", "test", "--quiet", "--test", test, *skip], copy, target).returncode != 0
                    results.append("test failure" if failed else "not detected")
            print(f"{arm:<12} {name:<44} {results[0]:<14} {results[1]}", flush=True)
            for path, text in originals.items():
                (copy / path).write_text(text)


if __name__ == "__main__":
    main()
