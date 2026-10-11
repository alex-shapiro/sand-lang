"""Richards: the classic operating-system scheduler simulation (V8 / "Are We Fast Yet" version).

Six tasks of four kinds pass packets through linked queues until the idle task runs out.
Stresses method calls, attribute mutation, None links, and virtual dispatch.
"""

from __future__ import annotations

ITERATIONS = 40
COUNT = 10000
DATA_SIZE = 4

ID_IDLE = 0
ID_WORKER = 1
ID_HANDLER_A = 2
ID_HANDLER_B = 3
ID_DEVICE_A = 4
ID_DEVICE_B = 5
NUMBER_OF_IDS = 6

KIND_DEVICE = 0
KIND_WORK = 1


class Packet:
    __slots__ = ("link", "id", "kind", "a1", "a2")

    def __init__(self, link: Packet | None, id: int, kind: int):
        self.link = link
        self.id = id
        self.kind = kind
        self.a1 = 0
        self.a2 = [0] * DATA_SIZE

    def add_to(self, queue: Packet | None) -> Packet:
        """Appends this packet to the end of `queue` and returns the queue's head."""
        self.link = None
        if queue is None:
            return self
        next = queue
        while (peek := next.link) is not None:
            next = peek
        next.link = self
        return queue


class Task:
    __slots__ = ()

    def run(self, s: Scheduler, packet: Packet | None) -> Tcb | None:
        raise NotImplementedError


class IdleTask(Task):
    __slots__ = ("v1", "count")

    def __init__(self, v1: int, count: int):
        self.v1 = v1
        self.count = count

    def run(self, s, packet):
        self.count -= 1
        if self.count == 0:
            return s.hold_current()
        if self.v1 & 1 == 0:
            self.v1 = self.v1 >> 1
            return s.release(ID_DEVICE_A)
        self.v1 = (self.v1 >> 1) ^ 0xD008
        return s.release(ID_DEVICE_B)


class WorkerTask(Task):
    __slots__ = ("dest", "v2")

    def __init__(self, dest: int, v2: int):
        self.dest = dest
        self.v2 = v2

    def run(self, s, packet):
        if packet is None:
            return s.suspend_current()
        self.dest = ID_HANDLER_B if self.dest == ID_HANDLER_A else ID_HANDLER_A
        packet.id = self.dest
        packet.a1 = 0
        for i in range(DATA_SIZE):
            self.v2 += 1
            if self.v2 > 26:
                self.v2 = 1
            packet.a2[i] = self.v2
        return s.queue(packet)


class HandlerTask(Task):
    __slots__ = ("work", "devices")

    def __init__(self):
        self.work: Packet | None = None
        self.devices: Packet | None = None

    def run(self, s, packet):
        if packet is not None:
            if packet.kind == KIND_WORK:
                self.work = packet.add_to(self.work)
            else:
                self.devices = packet.add_to(self.devices)
        work = self.work
        if work is not None:
            count = work.a1
            if count < DATA_SIZE:
                dev = self.devices
                if dev is not None:
                    self.devices = dev.link
                    dev.a1 = work.a2[count]
                    work.a1 = count + 1
                    return s.queue(dev)
            else:
                self.work = work.link
                return s.queue(work)
        return s.suspend_current()


class DeviceTask(Task):
    __slots__ = ("pending",)

    def __init__(self):
        self.pending: Packet | None = None

    def run(self, s, packet):
        if packet is not None:
            self.pending = packet
            return s.hold_current()
        v = self.pending
        if v is None:
            return s.suspend_current()
        self.pending = None
        return s.queue(v)


class Tcb:
    """A task control block: one task, its packet queue and its scheduling state."""

    __slots__ = ("link", "id", "priority", "queue", "task", "packet_pending", "waiting", "holding")

    def __init__(self, link: Tcb | None, id: int, priority: int, queue: Packet | None, task: Task):
        self.link = link
        self.id = id
        self.priority = priority
        self.queue = queue
        self.task = task
        self.packet_pending = queue is not None
        self.waiting = True
        self.holding = False

    def held_or_suspended(self) -> bool:
        return self.holding or (not self.packet_pending and self.waiting)

    def run(self, s: Scheduler) -> Tcb | None:
        packet = None
        if self.packet_pending and self.waiting and not self.holding:
            packet = self.queue
            self.queue = packet.link
            self.packet_pending = self.queue is not None
            self.waiting = False
        return self.task.run(s, packet)

    def check_priority_add(self, current: Tcb, packet: Packet) -> Tcb:
        """Adds `packet` to this task's queue; returns the task that should run next."""
        if self.queue is None:
            self.queue = packet
            self.packet_pending = True
            if self.priority > current.priority:
                return self
        else:
            self.queue = packet.add_to(self.queue)
        return current


class Scheduler:
    __slots__ = ("list", "blocks", "current", "current_id", "queue_count", "hold_count")

    def __init__(self):
        self.list: Tcb | None = None
        self.blocks: list[Tcb | None] = [None] * NUMBER_OF_IDS
        self.current: Tcb | None = None
        self.current_id = 0
        self.queue_count = 0
        self.hold_count = 0

    def add_task(self, id: int, priority: int, queue: Packet | None, task: Task) -> None:
        tcb = Tcb(self.list, id, priority, queue, task)
        self.list = tcb
        self.blocks[id] = tcb
        self.current = tcb

    def schedule(self) -> None:
        tcb = self.list
        while tcb is not None:
            if tcb.held_or_suspended():
                tcb = tcb.link
            else:
                self.current = tcb
                self.current_id = tcb.id
                tcb = tcb.run(self)

    def release(self, id: int) -> Tcb | None:
        tcb = self.blocks[id]
        if tcb is None:
            return None
        tcb.holding = False
        return tcb if tcb.priority > self.current.priority else self.current

    def hold_current(self) -> Tcb | None:
        self.hold_count += 1
        self.current.holding = True
        return self.current.link

    def suspend_current(self) -> Tcb | None:
        self.current.waiting = True
        return self.current

    def queue(self, packet: Packet) -> Tcb | None:
        t = self.blocks[packet.id]
        if t is None:
            return None
        self.queue_count += 1
        packet.link = None
        packet.id = self.current_id
        return t.check_priority_add(self.current, packet)


def make_queue(id: int, kind: int, n: int) -> Packet | None:
    queue = None
    for _ in range(n):
        queue = Packet(queue, id, kind)
    return queue


def run_richards() -> tuple[int, int]:
    s = Scheduler()
    s.add_task(ID_IDLE, 0, None, IdleTask(1, COUNT))
    s.current.waiting = False
    s.add_task(ID_WORKER, 1000, make_queue(ID_WORKER, KIND_WORK, 2), WorkerTask(ID_HANDLER_A, 0))
    s.add_task(ID_HANDLER_A, 2000, make_queue(ID_DEVICE_A, KIND_DEVICE, 3), HandlerTask())
    s.add_task(ID_HANDLER_B, 3000, make_queue(ID_DEVICE_B, KIND_DEVICE, 3), HandlerTask())
    s.add_task(ID_DEVICE_A, 4000, None, DeviceTask())
    s.add_task(ID_DEVICE_B, 5000, None, DeviceTask())
    s.schedule()
    return s.queue_count, s.hold_count


total_queued = 0
total_held = 0
for _ in range(ITERATIONS):
    queued, held = run_richards()
    assert queued == 23246 and held == 9297, f"wrong result: {queued} {held}"
    total_queued += queued
    total_held += held
print(f"iterations: {ITERATIONS}")
print(f"packets queued: {total_queued}")
print(f"holds: {total_held}")
