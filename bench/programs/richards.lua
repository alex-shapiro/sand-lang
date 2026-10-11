-- Richards: the classic operating-system scheduler simulation (V8 / "Are We Fast Yet" version).
-- Six tasks of four kinds pass packets through linked queues until the idle task runs out.
-- Stresses method calls, field mutation, nil links, and metatable dispatch.

local ITERATIONS = 40
local COUNT = 10000
local DATA_SIZE = 4

local ID_IDLE = 0
local ID_WORKER = 1
local ID_HANDLER_A = 2
local ID_HANDLER_B = 3
local ID_DEVICE_A = 4
local ID_DEVICE_B = 5

local KIND_DEVICE = 0
local KIND_WORK = 1

local Packet = {}
Packet.__index = Packet

function Packet.new(link, id, kind)
  local a2 = {}
  for i = 1, DATA_SIZE do a2[i] = 0 end
  return setmetatable({ link = link, id = id, kind = kind, a1 = 0, a2 = a2 }, Packet)
end

-- Appends this packet to the end of `queue` and returns the queue's head.
function Packet:add_to(queue)
  self.link = nil
  if queue == nil then return self end
  local next = queue
  local peek = next.link
  while peek ~= nil do
    next = peek
    peek = next.link
  end
  next.link = self
  return queue
end

local IdleTask = {}
IdleTask.__index = IdleTask

function IdleTask.new(v1, count)
  return setmetatable({ v1 = v1, count = count }, IdleTask)
end

function IdleTask:run(s, packet)
  self.count = self.count - 1
  if self.count == 0 then return s:hold_current() end
  if self.v1 & 1 == 0 then
    self.v1 = self.v1 >> 1
    return s:release(ID_DEVICE_A)
  else
    self.v1 = (self.v1 >> 1) ~ 0xD008
    return s:release(ID_DEVICE_B)
  end
end

local WorkerTask = {}
WorkerTask.__index = WorkerTask

function WorkerTask.new(dest, v2)
  return setmetatable({ dest = dest, v2 = v2 }, WorkerTask)
end

function WorkerTask:run(s, packet)
  if packet == nil then return s:suspend_current() end
  if self.dest == ID_HANDLER_A then self.dest = ID_HANDLER_B else self.dest = ID_HANDLER_A end
  packet.id = self.dest
  packet.a1 = 0
  local a2 = packet.a2
  for i = 1, DATA_SIZE do
    self.v2 = self.v2 + 1
    if self.v2 > 26 then self.v2 = 1 end
    a2[i] = self.v2
  end
  return s:queue(packet)
end

local HandlerTask = {}
HandlerTask.__index = HandlerTask

function HandlerTask.new()
  return setmetatable({ work = nil, devices = nil }, HandlerTask)
end

function HandlerTask:run(s, packet)
  if packet ~= nil then
    if packet.kind == KIND_WORK then
      self.work = packet:add_to(self.work)
    else
      self.devices = packet:add_to(self.devices)
    end
  end
  local work = self.work
  if work ~= nil then
    local count = work.a1
    if count < DATA_SIZE then
      local dev = self.devices
      if dev ~= nil then
        self.devices = dev.link
        dev.a1 = work.a2[count + 1]
        work.a1 = count + 1
        return s:queue(dev)
      end
    else
      self.work = work.link
      return s:queue(work)
    end
  end
  return s:suspend_current()
end

local DeviceTask = {}
DeviceTask.__index = DeviceTask

function DeviceTask.new()
  return setmetatable({ pending = nil }, DeviceTask)
end

function DeviceTask:run(s, packet)
  if packet ~= nil then
    self.pending = packet
    return s:hold_current()
  end
  local v = self.pending
  if v == nil then return s:suspend_current() end
  self.pending = nil
  return s:queue(v)
end

-- A task control block: one task, its packet queue and its scheduling state.
local Tcb = {}
Tcb.__index = Tcb

function Tcb.new(link, id, priority, queue, task)
  return setmetatable({
    link = link, id = id, priority = priority, queue = queue, task = task,
    packet_pending = queue ~= nil, waiting = true, holding = false,
  }, Tcb)
end

function Tcb:held_or_suspended()
  return self.holding or (not self.packet_pending and self.waiting)
end

function Tcb:run(s)
  local packet = nil
  if self.packet_pending and self.waiting and not self.holding then
    packet = self.queue
    self.queue = packet.link
    self.packet_pending = self.queue ~= nil
    self.waiting = false
  end
  return self.task:run(s, packet)
end

-- Adds `packet` to this task's queue; returns the task that should run next.
function Tcb:check_priority_add(current, packet)
  if self.queue == nil then
    self.queue = packet
    self.packet_pending = true
    if self.priority > current.priority then return self end
  else
    self.queue = packet:add_to(self.queue)
  end
  return current
end

local Scheduler = {}
Scheduler.__index = Scheduler

function Scheduler.new()
  return setmetatable({
    list = nil, blocks = {}, current = nil, current_id = 0, queue_count = 0, hold_count = 0,
  }, Scheduler)
end

function Scheduler:add_task(id, priority, queue, task)
  local tcb = Tcb.new(self.list, id, priority, queue, task)
  self.list = tcb
  self.blocks[id] = tcb
  self.current = tcb
end

function Scheduler:schedule()
  local tcb = self.list
  while tcb ~= nil do
    if tcb:held_or_suspended() then
      tcb = tcb.link
    else
      self.current = tcb
      self.current_id = tcb.id
      tcb = tcb:run(self)
    end
  end
end

function Scheduler:release(id)
  local tcb = self.blocks[id]
  if tcb == nil then return nil end
  tcb.holding = false
  if tcb.priority > self.current.priority then return tcb else return self.current end
end

function Scheduler:hold_current()
  self.hold_count = self.hold_count + 1
  self.current.holding = true
  return self.current.link
end

function Scheduler:suspend_current()
  self.current.waiting = true
  return self.current
end

function Scheduler:queue(packet)
  local t = self.blocks[packet.id]
  if t == nil then return nil end
  self.queue_count = self.queue_count + 1
  packet.link = nil
  packet.id = self.current_id
  return t:check_priority_add(self.current, packet)
end

local function make_queue(id, kind, n)
  local queue = nil
  for _ = 1, n do queue = Packet.new(queue, id, kind) end
  return queue
end

local function run_richards()
  local s = Scheduler.new()
  s:add_task(ID_IDLE, 0, nil, IdleTask.new(1, COUNT))
  s.current.waiting = false
  s:add_task(ID_WORKER, 1000, make_queue(ID_WORKER, KIND_WORK, 2), WorkerTask.new(ID_HANDLER_A, 0))
  s:add_task(ID_HANDLER_A, 2000, make_queue(ID_DEVICE_A, KIND_DEVICE, 3), HandlerTask.new())
  s:add_task(ID_HANDLER_B, 3000, make_queue(ID_DEVICE_B, KIND_DEVICE, 3), HandlerTask.new())
  s:add_task(ID_DEVICE_A, 4000, nil, DeviceTask.new())
  s:add_task(ID_DEVICE_B, 5000, nil, DeviceTask.new())
  s:schedule()
  return s.queue_count, s.hold_count
end

local total_queued, total_held = 0, 0
for _ = 1, ITERATIONS do
  local queued, held = run_richards()
  assert(queued == 23246 and held == 9297, "wrong result: " .. queued .. " " .. held)
  total_queued = total_queued + queued
  total_held = total_held + held
end
print("iterations: " .. ITERATIONS)
print("packets queued: " .. total_queued)
print("holds: " .. total_held)
