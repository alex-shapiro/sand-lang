# `day_of_week()` returns an int, so a wrong day number compiles and gives a wrong answer

TODO item 060 (Language): "Evaluate returning a `time.Weekday` enum instead of an `int`…"

**Pain.** `day_of_week()` returns `1` (Monday) to `7` (Sunday). Python numbers Monday as `0`, JavaScript numbers Sunday as `0`. If I bring either habit, my weekend check is wrong and `check` and `lint` do not warn me.

**Why it matters.** Off-by-one day bugs are silent: they show only on some dates. LLMs often write the Python form. A `Weekday` enum makes the wrong forms fail to compile and lets `match` check that all seven days are covered.

**Reproduce.**
```
sand check repro.sand && sand lint repro.sand    # ok, no warnings
sand run repro.sand
friday is weekend: true            # Python habit: `>= 5`
sunday detected: false             # JavaScript habit: `== 0`
friday is weekend (correct): false
```

**Aspirational.** `aspirational.sand` matches on `Saturday | Sunday`. Today it fails to compile because the result is an `int`.
