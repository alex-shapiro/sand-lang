"""A line-oriented TCP chat server.

CPython port of examples/chat_server.sand, using asyncio streams.

Usage: python3 chat_server.py 7000

Commands: `/nick NAME`, `/who`, `/quit`. Any other line is broadcast.
"""

import asyncio
import logging
import sys

log = logging.getLogger("chat")


class Client:
    __slots__ = ("id", "name", "reader", "writer")

    def __init__(self, id: int, name: str, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        self.id = id
        self.name = name
        self.reader = reader
        self.writer = writer


clients: dict[int, Client] = {}
next_id = 0


async def send(c: Client, text: str) -> None:
    # A client that has gone away is cleaned up by its own handler.
    try:
        c.writer.write((text + "\n").encode())
        await c.writer.drain()
    except ConnectionError:
        pass


async def broadcast(sender: int, text: str) -> None:
    for id, c in list(clients.items()):
        if id != sender:
            await send(c, text)


async def handle(c: Client) -> None:
    try:
        await send(c, f"welcome {c.name}")
        await broadcast(c.id, f"* {c.name} joined")
        while True:
            try:
                line = await c.reader.readline()
            except (ConnectionError, ValueError):
                break
            if not line:
                break
            text = line.decode().strip()
            if not text:
                continue
            if text.startswith("/nick "):
                old = c.name
                c.name = text.removeprefix("/nick ").strip()
                await broadcast(c.id, f"* {old} is now {c.name}")
                await send(c, f"you are now {c.name}")
            elif text == "/who":
                await send(c, f"online: {', '.join(x.name for x in clients.values())}")
            elif text == "/quit":
                break
            else:
                await broadcast(c.id, f"{c.name}: {text}")
    finally:
        # However the loop above ends, the client leaves.
        clients.pop(c.id, None)
        c.writer.close()
        await broadcast(c.id, f"* {c.name} left")
        log.info("client left name=%s", c.name)


async def on_connect(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    global next_id
    next_id += 1
    client = Client(next_id, f"guest{next_id}", reader, writer)
    clients[client.id] = client
    peer = writer.get_extra_info("peername")
    log.info("client joined peer=%s:%s", peer[0], peer[1])
    await handle(client)


async def main() -> None:
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 7000
    server = await asyncio.start_server(on_connect, "127.0.0.1", port)
    async with server:
        bound = server.sockets[0].getsockname()
        print(f"listening on {bound[0]}:{bound[1]}", flush=True)
        await server.serve_forever()


logging.basicConfig(format="%(levelname)s %(message)s", level=logging.INFO)
try:
    asyncio.run(main())
except KeyboardInterrupt:
    pass
