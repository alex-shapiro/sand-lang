# Tradeoffs

## One StdinFeed serves every session of an engine

`Config::stdin_feed` gives the same feed to every session and every run of an engine. A host with a pane for each session cannot tell which session a waiting read belongs to: `StdinReadId` has no session, and data from `send` goes to the reader that polls first. To share the feed safely, `stdin_feed.rs` keeps a list of readers, routes answers to a target read, and returns the oldest read from `awaited`.

Possible solutions:

- **Give each session its own standard input,** for example a session setter or `Engine::session_with`. Then a feed has one reader, and the reader list, the routing, and the "oldest read" rule go away. A host that wants one input for all sessions must then send to each feed.
- **Keep one shared feed, and add the session to `StdinReadId`.** A host can then route a prompt to the correct pane. The shared state stays.
- **Keep the current design.** A host that runs sessions at the same time must not depend on which session gets the input.
