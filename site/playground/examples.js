// Built-in playground examples verified by site/tests/playground.test.mjs.

export const examples = [
  {
    id: "welcome",
    title: "Welcome",
    files: {
      "main.sand": `//! Welcome to the Sand playground.
//!
//! Press Run (` + "`Ctrl-Enter` / `Cmd-Enter`" + `) to execute this program, then evaluate
//! expressions such as \`planets.len()\` or \`size(planets[4])\` in the console
//! below. Edit the Policy tab to configure capability rules and limits.

struct Planet {
    name: str
    moons: int
}

enum Size {
    Lonely
    Few(int)
    Many(int)
}

fn size(p: Planet) -> Size {
    match p.moons {
        0 => Size.Lonely
        n if n < 10 => Size.Few(n)
        n => Size.Many(n)
    }
}

let planets = [
    Planet(name: "Mercury", moons: 0),
    Planet(name: "Earth", moons: 1),
    Planet(name: "Mars", moons: 2),
    Planet(name: "Jupiter", moons: 95),
    Planet(name: "Saturn", moons: 146),
]

for p in planets {
    let description = match size(p) {
        Size.Lonely => "0 moons"
        Size.Few(n) => "\\(n) moon\\(if n == 1 { "" } else { "s" })"
        Size.Many(n) => "a crowd of \\(n) moons"
    }
    print("\\(p.name:<8) has \\(description)")
}

let total = planets.map(p => p.moons).sum()
let most = planets.max_by(p => p.moons).unwrap()
print("\\n\\(planets.len()) planets, \\(total) moons; \\(most.name) has the most.")
`,
    },
    policy: `# Allow standard output and set resource limits.
limit time 5s
limit memory 64M
`,
    expect: { ok: true, stdout: "5 planets, 244 moons; Saturn has the most." },
  },
  {
    id: "capabilities",
    title: "Capabilities tour",
    files: {
      "main.sand": `//! Programs can access only the capabilities permitted by the sandbox policy.
//! Operations not covered by an \`allow\` or \`deny\` rule in the Policy tab
//! trigger an interactive prompt above the console.

import env
import fs
import http
import io

fn report<T>(what: str, result: Result<T>) {
    match result {
        Ok(_) => print("allowed  \\(what)")
        Err(e) if e.kind == "permission_denied" => print("denied   \\(what): \\(e.message)")
        Err(e) => print("failed   \\(what): \\(e.message)")
    }
}

print("Files (configured in the file tabs above):")
report("read /data/greeting.txt", fs.read_text("/data/greeting.txt"))
report("read /data/private/key.txt", fs.read_text("/data/private/key.txt"))
report("read /data/../data/private/key.txt", fs.read_text("/data/../data/private/key.txt"))
report("write /out/note.txt", fs.create_dir("/out", recursive: true).and_then(_ => fs.write_text("/out/note.txt", "hello")))
report("write /data/greeting.txt", fs.write_text("/data/greeting.txt", "overwritten"))

print("\nEnvironment and standard input:")
print("         USER is \\(env.get("USER") ?? "not visible"), TOKEN is \\(env.get("TOKEN") ?? "not visible")")
report("read standard input", io.stdin.read_line())

print("\nNetwork:")
report("GET https://api.github.com/zen", http.get("https://api.github.com/zen"))
report("GET https://en.wikipedia.org/api/rest_v1/page/summary/Sand", http.get("https://en.wikipedia.org/api/rest_v1/page/summary/Sand"))
report("GET http://192.168.0.1/ (a private address)", http.get("http://192.168.0.1/"))
`,
      "data/greeting.txt": "Hello from a file!\n",
      "data/private/key.txt": "the key to everything\n",
    },
    policy: `# File access rules.
allow read /data
deny read /data/private
allow write /out

# Environment and standard input rules.
allow env USER
allow stdin

# Network rules.
allow http GET api.github.com

limit time 10s
`,
    env: "USER=sandy\nTOKEN=s3cret-token\n",
    stdin: "a line from the Input tab\n",
    expect: {
      ok: true,
      prompts: 4,
      stdout: [
        "allowed  read /data/greeting.txt",
        "denied   read /data/private/key.txt: permission denied",
        "denied   read /data/../data/private/key.txt",
        "allowed  write /out/note.txt",
        "denied   write /data/greeting.txt",
        "USER is sandy, TOKEN is not visible",
        "allowed  read standard input",
        "allowed  GET https://api.github.com/zen",
        "denied   GET https://en.wikipedia.org/api/rest_v1/page/summary/Sand: permission denied: GET https://en.wikipedia.org/api/rest_v1/page/summary/Sand",
        "denied   GET http://192.168.0.1/ (a private address): permission denied",
      ],
      writes: ["/out/note.txt"],
    },
  },
  {
    id: "network",
    title: "Network",
    files: {
      "main.sand": `//! Queries public HTTP APIs using \`http\` and \`json\`.

import http
import json

struct Repo {
    full_name: str
    description: str?
    stargazers_count: int
    language: str?
}

struct Page {
    title: str
    extract: str
}

struct Weather {
    current: Current
}

struct Current {
    temperature_2m: float
}

let response = http.get("https://api.github.com/repos/rust-lang/rust")?
if response.ok() {
    let repo: Repo = json.decode(response.text()?)?
    print("\\(repo.full_name): \\(repo.description ?? "(none)")")
    print("  \\(repo.stargazers_count) stars, written in \\(repo.language ?? "?")")
} else {
    print("GitHub answered \\(response.status)")
}

let page: Page = json.decode(http.get("https://en.wikipedia.org/api/rest_v1/page/summary/Sand")?.text()?)?
print("\\(page.title): \\(page.extract.split(". ")[0]).")

match http.get("https://api.open-meteo.com/v1/forecast?latitude=52.52&longitude=13.41&current=temperature_2m") {
    Ok(r) => {
        let weather: Weather = json.decode(r.text()?)?
        print("Berlin: \\(weather.current.temperature_2m) °C")
    }
    Err(e) => print("api.open-meteo.com: \\(e.message)")
}
`,
    },
    policy: `# Allow GET requests matching these URL patterns.
allow http GET api.github.com/repos/**
allow http GET en.wikipedia.org/api/rest_v1/page/summary/*
limit time 20s
`,
    expect: { ok: true, prompts: 1, stdout: ["rust-lang/rust", "Sand: ", "api.open-meteo.com: permission denied: GET https://api.open-meteo.com/v1/forecast"] },
  },
  {
    id: "files",
    title: "Files and the policy",
    files: {
      "main.sand": `//! Aggregates CSV sales data from \`/data/sales.csv\` and writes a report to
//! \`/out/report.txt\`.

import fs

struct Sale {
    region: str
    units: int
    price: float
}

fn parse(line: str) -> Result<Sale> {
    let cols = line.split(",")
    if cols.len() != 4 {
        return Err(Error("expected 4 columns: \\(line)", kind: "parse"))
    }
    let units = cols[2].parse_int().ok_or(Error("bad units: \\(cols[2])"))?
    let price = cols[3].parse_float().ok_or(Error("bad price: \\(cols[3])"))?
    Ok(Sale(region: cols[0], units: units, price: price))
}

let totals: [str: float] = [:]
{
    use sales = fs.open("/data/sales.csv")?
    sales.read_line()?
    while let Some(line) = sales.read_line()? {
        let s = parse(line)?
        totals[s.region] = totals.get_or(s.region, 0.0) + s.units.to_float() * s.price
    }
}

var report = "Revenue by region\\n"
for (region, total) in totals.entries().sorted_by(e => -e.1) {
    report += "  \\(region:<6) \\(total:>8.2)\\n"
}
print(report)

match fs.create_dir("/out", recursive: true).and_then(_ => fs.write_text("/out/report.txt", report)) {
    Ok(()) => print("Wrote /out/report.txt (listed below the output).")
    Err(e) => print("Could not write the report: \\(e.message)")
}

match fs.read_text("/data/private/salaries.csv") {
    Ok(text) => print("Read the private file: \\(text.len()) bytes")
    Err(e) => print("Private data stays private: \\(e.message)")
}
`,
      "data/sales.csv": `region,product,units,price
north,widget,12,2.50
south,widget,7,2.50
north,gadget,3,10.00
east,gizmo,20,1.25
south,gadget,5,10.00
east,widget,9,2.50
`,
      "data/private/salaries.csv": `name,salary
ada,1000000
`,
    },
    policy: `allow read /data
deny read /data/private
allow write /out
limit time 5s
`,
    expect: { ok: true, stdout: "Private data stays private: permission denied", writes: ["/out/report.txt"] },
  },
  {
    id: "tasks",
    title: "Tasks and channels",
    files: {
      "main.sand": `//! Processes jobs concurrently across three worker tasks using typed channels.

import time

let jobs: Channel<int> = Channel()
let results: Channel<str> = Channel()
let start = time.Stopwatch()

fn worker(name: str, jobs: Channel<int>, results: Channel<str>) {
    for n in jobs.iter() {
        task.sleep(n * 40ms)
        results.send("\\(start.elapsed().millis():>4) ms  \\(name) finished job \\(n)")
    }
}

let workers = ["ant", "bee", "cat"].map(name => spawn worker(name, jobs, results))
for n in [5, 1, 3, 2, 4, 1] {
    jobs.send(n)
}
jobs.close()

spawn {
    task.all(workers)
    results.close()
}
for line in results.iter() {
    print(line)
}
print("\\(start.elapsed().millis():>4) ms  all jobs done")
`,
    },
    policy: `limit time 10s
`,
    expect: { ok: true, stdout: "all jobs done" },
  },
  {
    id: "limits",
    title: "Limits stop runaway code",
    files: {
      "main.sand": `//! Demonstrates how resource limits terminate an infinite loop.

let primes: [int] = []
var n = 2
while true {
    if primes.all(p => p * p > n or n % p != 0) {
        primes.push(n)
        if primes.len() % 2000 == 0 {
            print("\\(primes.len()) primes so far, the last is \\(n)")
        }
    }
    n += 1
}
`,
    },
    policy: `limit time 1s
limit memory 16M
`,
    expect: { ok: false, exitCode: 3, error: "time limit exceeded" },
  },
  {
    id: "prompt",
    title: "Ask before access",
    files: {
      "main.sand": `//! Prompts for permission before reading environment variables or files.

import env
import fs

print("Hello, \\(env.get("USER") ?? "stranger")!")
for path in ["/notes/todo.txt", "/notes/diary.txt"] {
    match fs.read_text(path) {
        Ok(text) => print("\\(path): \\(text.trim())")
        Err(e) => print("\\(path): \\(e.message)")
    }
}
`,
      "notes/todo.txt": "Water the plants.\n",
      "notes/diary.txt": "Dear diary, today I wrote a sandbox.\n",
    },
    policy: `limit time 5s
`,
    env: "USER=sandy\n",
    expect: { ok: true, prompts: 3, answer: "once" },
  },
  {
    id: "json",
    title: "JSON, regex and types",
    files: {
      "main.sand": `//! Decodes JSON from standard input into typed structs and extracts integers
//! with a regular expression.

import io
import json
import regex

struct Order {
    id: int
    customer: str
    items: [Item]
}

struct Item {
    name: str
    quantity: int
    note: str?
}

let orders: [Order] = json.decode(io.stdin.read_text()?)?
for order in orders {
    let count = order.items.map(i => i.quantity).sum()
    print("order #\\(order.id) for \\(order.customer): \\(count) items")
    for item in order.items.filter(i => i.note != None) {
        print("  note on \\(item.name): \\(item.note.unwrap())")
    }
}

let digits = regex.Regex(r"\\d+")?
let text = "3 apples, 12 pears and 7 plums"
let numbers = digits.find_all(text).map(m => m.text.parse_int().unwrap())
print("\\(text) -> \\(numbers) (sum \\(numbers.sum()))")
print(json.encode(orders[0].items[0]))
`,
    },
    policy: `allow stdin
limit time 5s
`,
    stdin: `[
  {"id": 1, "customer": "Ada", "items": [
    {"name": "tea", "quantity": 2, "note": "green"},
    {"name": "scones", "quantity": 6}
  ]},
  {"id": 2, "customer": "Grace", "items": [
    {"name": "coffee", "quantity": 1}
  ]}
]
`,
    expect: { ok: true, stdout: "order #2 for Grace: 1 items" },
  },
  {
    id: "traits",
    title: "Traits and generics",
    files: {
      "main.sand": `//! Defines a trait with a default method and uses it as a generic bound.

trait Shape {
    fn area(self) -> float
    fn name(self) -> str

    fn describe(self) -> str { "\\(self.name()) with area \\(self.area():.2)" }
}

struct Circle { r: float }
struct Rect { w: float, h: float }

impl Shape for Circle {
    fn area(self) -> float { 3.14159 * self.r * self.r }
    fn name(self) -> str { "circle" }
}

impl Shape for Rect {
    fn area(self) -> float { self.w * self.h }
    fn name(self) -> str { if self.w == self.h { "square" } else { "rectangle" } }
}

fn largest<T: Shape>(shapes: [T]) -> T? {
    shapes.max_by(s => s.area())
}

let circles = [Circle(r: 1.0), Circle(r: 2.5), Circle(r: 0.5)]
let rects = [Rect(w: 2.0, h: 3.0), Rect(w: 4.0, h: 4.0)]
for c in circles { print(c.describe()) }
for r in rects { print(r.describe()) }
print("largest circle: \\(largest(circles).unwrap().describe())")
print("largest rectangle: \\(largest(rects).unwrap().describe())")
`,
    },
    policy: `limit time 5s
`,
    expect: { ok: true, stdout: "largest rectangle: square with area 16.00" },
  },
];
