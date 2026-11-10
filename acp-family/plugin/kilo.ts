/**
 * Steery's plugin for Kilo: the door of PROTOCOL.md §4.4, version 1. Kilo loads it in every
 * process that runs its server (`kilo run`, `kilo acp`, `kilo serve`, the TUI's daemon), and
 * for each root conversation that comes alive there it dials the runtime's loopback door,
 * says what the conversation is doing (`state`, `live`), and answers the runtime's asks
 * through the in-process Kilo client.
 *
 * Kilo keeps its record in `kilo.db`, rows updated in place, which the driver reads, so this
 * door carries no entries. Kilo's own `task` sub-agents open no door: their calls show as
 * the children of the lead's `task` call.
 */
import type { Hooks, Plugin, PluginInput } from "@kilocode/plugin";
import { spawnSync } from "node:child_process";

/** The runtime's door, filled in by the driver's `setup/plan`. */
const DOOR = "__STEERY_DOOR__";
const HARNESS = "kilo";
const DOOR_VERSION = 1;

const REDIAL_MIN_MS = 250;
const REDIAL_MAX_MS = 2000;
const OTHER_DOOR_MS = 5000;
const HELLO_UNREADABLE = 4400;
const OTHER_DOOR = 4409;
const REPLACED = 4410;
const LET_GO = 1000;

const STATE_EVERY_MS = 100;
const LIVE_EVERY_MS = 16;
/** Under the door's 64 KiB, with room for the frame around the parts. */
const LIVE_TEXT_MAX = (64 << 10) - 2048;
const ANSWERS_KEPT = 256;
const ENDED_KEPT = 64;
const SETTLED_KEPT = 16;
/** How long handed words may take to show as held before the say is `no-answer`. */
const HELD_WITHIN_MS = 8000;
const STOPPED_WITHIN_MS = 2500;
const SETTLE_WAIT_MS = 1000;
const ERROR_MAX = 600;

/** Kilo names an MCP tool `<server>_<tool>`; Steery's server is this one. */
const STEERY = "steery";

type Json = Record<string, unknown>;
type Fields = { readonly [key: string]: unknown };
type Client = PluginInput["client"];
type Mode = "tui" | "rpc" | "json" | "print";
type Answer = { ok: unknown } | { refused: string; error?: string };
type Model = { providerID: string; modelID: string; variant?: string };

/**
 * `STEERY_STARTED=<pid>:<what>`, trusted where this process's parent is that pid. Kilo's npm
 * package installs `kilo` as a node script that spawns the real binary, so where the parent
 * is that launcher (`node …/kilo`), the launcher's own parent counts as the starter.
 */
function startedBy(): string | undefined {
  const value = process.env.STEERY_STARTED;
  const at = value?.indexOf(":") ?? -1;
  if (!value || at < 1) return undefined;
  const pid = Number(value.slice(0, at));
  if (pid <= 1) return undefined;
  if (pid === process.ppid) return value.slice(at + 1);
  try {
    const ps = spawnSync("ps", ["-o", "ppid=,command=", "-p", String(process.ppid)], { encoding: "utf8" });
    const [, parent, command] = /^\s*(\d+)\s+(.*)$/.exec(ps.stdout.trim()) ?? [];
    const launcher = /^(\S*\/)?node\s+(\S*\/)?kilo(\s|$)/.test(command ?? "");
    return launcher && Number(parent) === pid ? value.slice(at + 1) : undefined;
  } catch {
    return undefined;
  }
}

/**
 * Who is at this process, from how it was started. A turn this driver started runs `kilo
 * acp` for that turn alone. The TUI starts no server of its own: its sessions run in a
 * daemon it spawns as `kilo serve` with `KILOCODE_FEATURE=daemon`
 * (`kilocode/daemon/daemon.ts`), so that `serve` is the TUI's; any other `serve`, `acp`
 * and `web` serve programs; with no command, the TUI runs its server in a worker of this
 * same process, whose argv is this one's.
 */
function modeOf(started: string | undefined): Mode {
  if (started?.startsWith("turn:") || started?.startsWith("turn@")) return "json";
  const args = process.argv.slice(2);
  switch (args.find((arg) => !arg.startsWith("-"))) {
    case "run": {
      const at = args.indexOf("--format");
      return args[at + 1] === "json" && at >= 0 || args.includes("--format=json") ? "json" : "print";
    }
    case "serve":
      return process.env.KILOCODE_FEATURE === "daemon" ? "tui" : "rpc";
    case "acp":
    case "web":
    case "daemon":
      return "rpc";
    default:
      return "tui";
  }
}

/** A Kilo id of its own kind: `<prefix>_` and an ascending time, as Kilo mints them. */
let lastMinted = 0;
let minted = 0;
function mint(prefix: string): string {
  const now = Date.now();
  minted = now === lastMinted ? minted + 1 : 1;
  lastMinted = now;
  const value = BigInt(now) * 0x1000n + BigInt(minted);
  const time = value.toString(16).padStart(12, "0").slice(-12);
  const chars = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
  const bytes = crypto.getRandomValues(new Uint8Array(14));
  return `${prefix}_${time}${Array.from(bytes, (byte) => chars[byte % 62]).join("")}`;
}

/** A Steery operation a shell command runs, as pi's plugin reads it: `steery <resource> <verb> …`. */
type Ran = { op: string; words: string };
function ran(command: unknown): Ran | undefined {
  if (typeof command !== "string") return undefined;
  const found = command.match(/(?:^|[\s;&|(`'"\/])steery\s+([a-z][a-z_-]*)(?:\s+([a-z][a-z_-]*))?([^;&|\n]*)/);
  if (!found) return undefined;
  const [, resource, verb = "", rest = ""] = found;
  if (resource === "request") return { op: "requests.create", words: `${verb} ${rest}` };
  return { op: `${resource}.${verb}`, words: rest };
}

/**
 * Whether a Steery call leaves its turn waiting: a `resources get` that waits, a `resources
 * act` watch, or one with a queue whose row says it joined the queue.
 */
function waits({ op, words }: Ran, output: string): boolean {
  if (op === "resources.get") return /--wait\b|"wait_secs"\s*:\s*[1-9]/.test(words);
  if (op !== "resources.act") return false;
  if (/--action[=\s]+["']?watch\b|"action"\s*:\s*"watch"/.test(words)) return true;
  if (!/--queue\b|"queue"\s*:\s*true/.test(words)) return false;
  try {
    const acted = JSON.parse(output.slice(output.indexOf("{"), output.lastIndexOf("}") + 1));
    return acted?.row?.params?.queue === true;
  } catch {
    return false;
  }
}

/** How many leading Unicode scalar values `a` and `b` share. */
function shared(a: string[], b: string[]): number {
  let at = 0;
  while (at < a.length && at < b.length && a[at] === b[at]) at++;
  return at;
}

/**
 * An object's fields, else none. Kilo's bus carries events that its SDK's types do not list
 * (`message.part.delta`, `session.queue.changed`, `permission.asked`), and the door's frames
 * are the runtime's, so the plugin checks every field it reads.
 */
function fields(value: unknown): Fields {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Fields) : {};
}

const text = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined);
const count = (value: unknown): number | undefined => (typeof value === "number" && Number.isFinite(value) ? value : undefined);

function modelOf(value: unknown): Model | undefined {
  const model = fields(value);
  const providerID = text(model.providerID);
  const modelID = text(model.modelID);
  if (!providerID || !modelID) return undefined;
  const variant = text(model.variant);
  return { providerID, modelID, ...(variant ? { variant } : {}) };
}

type Failure = { name?: string; line?: string };

function failureOf(value: unknown): Failure | undefined {
  if (value === undefined || value === null) return undefined;
  const error = fields(value);
  const line = text(fields(error.data).message) || text(error.message) || text(error.name);
  return { name: text(error.name), ...(line ? { line: line.slice(0, ERROR_MAX) } : {}) };
}

/** The tokens a response used, its whole context. */
function usedOf(value: unknown): number | undefined {
  const tokens = fields(value);
  const total = count(tokens.total);
  if (total) return total;
  const cache = fields(tokens.cache);
  const sum = [tokens.input, tokens.output, tokens.reasoning, cache.read, cache.write].reduce<number>(
    (all, part) => all + (count(part) ?? 0),
    0,
  );
  return sum || undefined;
}

/** A Kilo message's info, as far as the door reads it. */
type Info = {
  id: string;
  role: string;
  parentID?: string;
  created?: number;
  completed?: number;
  compacting?: number;
  /** An assistant's model; a user message's is `model`. */
  providerID?: string;
  modelID?: string;
  variant?: string;
  model?: Model;
  finish?: string;
  error?: Failure;
  summary: boolean;
  used?: number;
};

function infoOf(value: unknown): Info | undefined {
  const info = fields(value);
  const id = text(info.id);
  const role = text(info.role);
  if (!id || !role) return undefined;
  const time = fields(info.time);
  return {
    id,
    role,
    parentID: text(info.parentID),
    created: count(time.created),
    completed: count(time.completed),
    providerID: text(info.providerID),
    modelID: text(info.modelID),
    variant: text(info.variant),
    model: modelOf(info.model),
    finish: text(info.finish),
    error: failureOf(info.error),
    summary: info.summary === true,
    used: usedOf(info.tokens),
  };
}

/** A Kilo part, as far as the door reads it. */
type PartOf = {
  id: string;
  messageID: string;
  type: string;
  text: string;
  tool: string;
  callID: string;
  status?: string;
  start?: number;
  end?: number;
  /** The session a `task` call runs its sub-agent in. */
  child?: string;
};

function partOf(value: unknown): PartOf | undefined {
  const part = fields(value);
  const id = text(part.id);
  const messageID = text(part.messageID);
  const type = text(part.type);
  if (!id || !messageID || !type) return undefined;
  const state = fields(part.state);
  const time = fields(state.time);
  return {
    id,
    messageID,
    type,
    text: text(part.text) ?? "",
    tool: text(part.tool) ?? "",
    callID: text(part.callID) ?? id,
    status: text(state.status),
    start: count(time.start),
    end: count(time.end),
    child: text(fields(state.metadata).sessionId),
  };
}

/** How a message stopped, as the door's `live.end.stop` names it. */
function stopOf(info: Info): string {
  if (info.error?.name === "MessageAbortedError") return "aborted";
  if (info.error) return "error";
  if (info.finish === "length") return "length";
  if (info.finish === "tool-calls") return "tool_use";
  return "stop";
}

/** Words Steery says into the conversation, marked as Steery's. */
const reminder = (words: string) => `<system-reminder reason="steery">\n${words}\n</system-reminder>`;

/** Words of a `say`, from the ask until Kilo takes them or lets them go. */
type Say = {
  id: string;
  also: string[];
  words: string;
  mode: "steer" | "queue";
  thrown?: string;
  /** Kilo's id for the user message that carries them, once handed. */
  message?: string;
  queuedAt?: number;
  takenAt?: number;
  /** Handed while a turn ran: Kilo queues them until the turn yields. */
  steered: boolean;
  /** Seen in Kilo's queue. */
  queued: boolean;
  letGo: boolean;
  modelMissing?: string;
  handedAgain: boolean;
  wake: Array<() => void>;
};

type Message = { info: Info; parts: Map<string, { type: string; text: string }> };

/** A model as Kilo lists it: its context window and the variants (thinking levels) it has. */
type Listed = { context?: number; variants: string[] };

type Live = { first: number; at: number; sent: Record<string, string[]>; timer?: ReturnType<typeof setTimeout>; ended: boolean };

/** What the conversations of this process share: Kilo's MCP servers and its models. */
type Shared = {
  servers?: Promise<string[]>;
  models?: Promise<Map<string, Listed>>;
  parents: Map<string, Promise<string | undefined>>;
  doors: Map<string, Door>;
  children: Map<string, string>;
};

const process_: Shared = (() => {
  const key = Symbol.for("steery.kilo");
  const global = globalThis as unknown as Record<symbol, Shared | undefined>;
  return (global[key] ??= { parents: new Map(), doors: new Map(), children: new Map() });
})();

/** One root conversation of this process, and its door. */
class Door {
  socket: WebSocket | undefined;
  welcomed = false;
  live = false;
  wanted = true;
  unheard = false;
  backoff = REDIAL_MIN_MS;
  redial: ReturnType<typeof setTimeout> | undefined;
  ending = false;

  status = "idle";
  retry: Json | undefined;
  compacting = false;
  turn: { began_at: number; says: string[]; steerys: boolean; first?: string } | undefined;
  /** The user message Kilo's assistant answers last. */
  parent: string | undefined;
  turns = 0;
  queue: string[] = [];
  held: Say[] = [];
  says = new Map<string, Say>();
  byMessage = new Map<string, Say>();
  throws = new Map<string, Say>();
  answers = new Map<string, Promise<Answer>>();
  picked: Model | undefined;
  messages = new Map<string, Message>();
  calls = new Map<string, { tool: string; since: number; child?: string }>();
  childCalls = new Map<string, Map<string, string>>();
  ended: Array<{ id: string; at: number }> = [];
  dialogs = new Map<string, { kind: string; title: string; since: number }>();
  settled: Json[] = [];
  lives = new Map<string, Live>();
  asked = false;
  parked = false;
  waiting = false;
  continued = false;
  continuations = new Set<string>();
  settling = new Map<string, (ok: unknown) => void>();
  settledMessages = new Set<string>();
  sentState = "";
  stateTimer: ReturnType<typeof setTimeout> | undefined;
  stateAt = 0;

  constructor(
    readonly id: string,
    readonly client: Client,
    readonly mode: Mode,
    readonly lead: { harness: string; external_id: string } | undefined,
    readonly cwd: string,
  ) {}

  oneShot(): boolean {
    return this.mode === "json" || this.mode === "print";
  }

  busy(): boolean {
    return this.status !== "idle" || this.turn !== undefined;
  }

  pending(): boolean {
    return this.held.length > 0 || [...this.says.values()].some((say) => say.message && !say.takenAt && !say.letGo);
  }

  idle(): boolean {
    return !this.busy() && !this.pending() && !this.compacting;
  }

  // ---- the connection -------------------------------------------------------------

  dial(): void {
    clearTimeout(this.redial);
    this.redial = undefined;
    if (!this.wanted || this.unheard || this.socket) return;
    let socket: WebSocket;
    try {
      socket = new WebSocket(DOOR);
    } catch {
      this.dropped(undefined);
      return;
    }
    this.socket = socket;
    socket.onopen = () => this.send(this.hello());
    socket.onmessage = (message) => this.arrived(message.data);
    socket.onclose = (closed) => {
      if (this.socket === socket) this.dropped(closed.code);
    };
    socket.onerror = () => {};
  }

  hello(): Json {
    return {
      t: "hello",
      door: DOOR_VERSION,
      harness: HARNESS,
      pid: process.pid,
      session_id: this.id,
      mode: this.mode,
      ...(this.lead ? { lead: this.lead } : {}),
      state: this.state(),
      cwd: this.cwd,
    };
  }

  dropped(code: number | undefined): void {
    const was = this.welcomed;
    this.socket = undefined;
    this.welcomed = false;
    this.live = false;
    for (const wake of this.settling.values()) wake(undefined);
    this.settling.clear();
    if (code === HELLO_UNREADABLE) this.unheard = true;
    if (!this.wanted || this.unheard || code === REPLACED) return;
    if (was) this.backoff = REDIAL_MIN_MS;
    const wait = code === OTHER_DOOR ? OTHER_DOOR_MS : this.backoff;
    this.backoff = Math.min(this.backoff * 2, REDIAL_MAX_MS);
    this.redial = setTimeout(() => this.dial(), wait);
    (this.redial as { unref?: () => void }).unref?.();
  }

  /** Close the door for good: the conversation ended here. */
  shut(): void {
    this.wanted = false;
    this.ending = true;
    clearTimeout(this.redial);
    const socket = this.socket;
    this.socket = undefined;
    try {
      socket?.close(LET_GO);
    } catch {
      // Already closed.
    }
  }

  send(frame: Json): void {
    try {
      if (this.socket?.readyState === WebSocket.OPEN) this.socket.send(JSON.stringify(frame));
    } catch {
      // A broken socket closes, and the close redials.
    }
  }

  arrived(data: unknown): void {
    let parsed: unknown;
    try {
      parsed = JSON.parse(String(data));
    } catch {
      return;
    }
    const frame = fields(parsed);
    if (frame.t === "welcome") {
      if (frame.door !== DOOR_VERSION) return;
      this.welcomed = true;
      this.live = frame.live === true;
      this.backoff = REDIAL_MIN_MS;
      if (frame.entries) {
        this.send({ t: "lost", why: "Kilo keeps its record in kilo.db, which the driver reads; this door carries no entries" });
      }
      this.sentState = "";
      this.noteState();
      for (const message of this.lives.keys()) this.flushLive(message);
      return;
    }
    if (frame.t === "answer" && typeof frame.id === "string") {
      this.settling.get(frame.id)?.(frame.ok);
      return;
    }
    if (frame.t === "ask" && typeof frame.id === "string" && this.welcomed) {
      const id: string = frame.id;
      let answer = this.answers.get(id);
      if (!answer) {
        answer = this.ask(frame).catch((error: unknown): Answer => ({ refused: "refused", error: String(error) }));
        this.answers.set(id, answer);
        while (this.answers.size > ANSWERS_KEPT) this.answers.delete(this.answers.keys().next().value as string);
      }
      void answer.then((said) => {
        // Only `no-answer` and answers that took are kept; any other refusal is forgotten.
        if ("refused" in said && said.refused !== "no-answer") this.answers.delete(id);
        this.send({ t: "answer", id, ...said });
      });
    }
  }

  // ---- state ----------------------------------------------------------------------

  model(): Model | undefined {
    for (const { info } of [...this.messages.values()].reverse()) {
      if (info.role === "assistant" && info.providerID && info.modelID) {
        return { providerID: info.providerID, modelID: info.modelID, ...(info.variant ? { variant: info.variant } : {}) };
      }
      if (info.role === "user" && info.model) return info.model;
    }
    return undefined;
  }

  async contextLeft(model: Model | undefined): Promise<number | undefined> {
    if (!model) return undefined;
    const used = [...this.messages.values()].reverse().find(({ info }) => info.role === "assistant" && info.used)?.info.used;
    const limit = (await models(this.client)).get(`${model.providerID}/${model.modelID}`)?.context;
    if (!used || !limit || limit <= 0) return undefined;
    return Math.max(0, Math.min(1, (limit - used) / limit));
  }

  left: number | undefined;

  state(): Json {
    const model = this.model();
    const calls = [...this.calls.entries()].map(([id, call]) => ({
      id,
      tool: call.tool,
      since: call.since,
      children: [...(call.child ? this.childCalls.get(call.child)?.entries() ?? [] : [])].map(([id, tool]) => ({ id, tool })),
    }));
    const dialog = this.mode === "tui" ? [...this.dialogs.values()][0] : undefined;
    return {
      ...(this.turn ? { turn: { began_at: this.turn.began_at, says: this.turn.says } } : {}),
      idle: this.idle(),
      ...(this.pending() ? { pending: true } : {}),
      // A one-shot run has no editor anywhere: nobody types into it.
      ...(this.oneShot() ? { typing: false } : {}),
      compacting: this.compacting,
      retrying: this.status === "retry",
      ...(this.status === "retry" && this.retry ? { retry: this.retry } : {}),
      ...(model ? { model: `${model.providerID}/${model.modelID}` } : {}),
      ...(model?.variant ? { thinking: model.variant } : {}),
      ...(this.left !== undefined ? { context_left: this.left } : {}),
      calls,
      ended: this.ended,
      ...(dialog ? { dialog } : {}),
      settled: this.settled,
    };
  }

  noteState(): void {
    if (this.stateTimer || !this.welcomed) return;
    const wait = Math.max(0, this.stateAt + STATE_EVERY_MS - Date.now());
    this.stateTimer = setTimeout(() => {
      this.stateTimer = undefined;
      if (!this.welcomed) return;
      const state = this.state();
      const text = JSON.stringify(state);
      if (text === this.sentState) return;
      this.sentState = text;
      this.stateAt = Date.now();
      this.send({ t: "state", state });
    }, wait);
  }

  async refreshLeft(): Promise<void> {
    try {
      this.left = await this.contextLeft(this.model());
    } catch {
      this.left = undefined;
    }
    this.noteState();
  }

  // ---- live -----------------------------------------------------------------------

  noteLive(message: string): void {
    const live = this.lives.get(message);
    if (!live || live.timer || !this.live) return;
    const wait = Math.max(0, live.at === live.first && !Object.keys(live.sent).length ? 0 : LIVE_EVERY_MS);
    live.timer = setTimeout(() => {
      live.timer = undefined;
      this.flushLive(message);
    }, wait);
  }

  voices(message: Message): Record<string, string[]> {
    const texts: Record<string, string[]> = { agent: [], thought: [] };
    const ids = [...message.parts.keys()].sort();
    for (const voice of ["agent", "thought"] as const) {
      const type = voice === "agent" ? "text" : "reasoning";
      const blocks = ids
        .map((id) => message.parts.get(id)!)
        .filter((part) => part.type === type && part.text.trim())
        .map((part) => part.text);
      texts[voice] = Array.from(blocks.join("\n"));
    }
    return texts;
  }

  flushLive(id: string): void {
    const live = this.lives.get(id);
    const message = this.messages.get(id);
    if (!live || !message || !this.live || live.ended) return;
    const now = this.voices(message);
    const parts: Json[] = [];
    let room = LIVE_TEXT_MAX;
    let cut = false;
    for (const voice of ["thought", "agent"]) {
      const sent = live.sent[voice] ?? [];
      const text = now[voice];
      if (text.length === sent.length && shared(text, sent) === text.length) continue;
      const from = shared(text, sent);
      let tail = text.slice(from);
      let bytes = Buffer.byteLength(tail.join(""));
      while (bytes > room && tail.length) {
        tail = tail.slice(0, Math.max(0, Math.floor(tail.length * (room / bytes)) - 1));
        bytes = Buffer.byteLength(tail.join(""));
        cut = true;
      }
      room -= bytes;
      live.sent[voice] = [...text.slice(0, from), ...tail];
      parts.push({ voice, from, text: tail.join("") });
    }
    const info = message.info;
    const done = !!info.completed && !cut;
    const forming = [...message.parts.values()].find((part) => part.type === "tool-pending")?.text;
    if (!parts.length && !done) return;
    this.send({
      t: "live",
      message: id,
      at: live.at,
      first_at: live.first,
      parts,
      ...(forming ? { forming } : {}),
      ...(done ? { end: { stop: stopOf(info) } } : {}),
    });
    if (done) {
      live.ended = true;
      this.lives.delete(id);
    } else if (cut) this.noteLive(id);
  }

  // ---- what Kilo says -------------------------------------------------------------

  /** A turn begins: the status went busy from idle. */
  began(): void {
    if (this.turn) return;
    this.turn = { began_at: Date.now(), says: [], steerys: false };
    this.turns++;
  }

  /** A turn ends: Kilo is idle again. */
  endedTurn(): void {
    const turn = this.turn;
    this.turn = undefined;
    if (turn?.steerys) {
      const last = [...this.messages.values()].reverse().find(({ info }) => info.role === "assistant");
      const failure = last?.info.error?.name === "MessageAbortedError" ? undefined : last?.info.error;
      this.settled.push({
        began_at: turn.began_at,
        at: Date.now(),
        says: turn.says,
        failed: !!failure,
        ...(failure?.line ? { error: failure.line } : {}),
      });
      while (this.settled.length > SETTLED_KEPT) this.settled.shift();
    }
    // Words Kilo let go with the turn: handed, not taken, out of its queue.
    for (const say of this.says.values()) {
      if (say.message && !say.takenAt && !this.queue.includes(say.message) && say.queued) {
        say.letGo = true;
        for (const wake of say.wake.splice(0)) wake();
      }
    }
    void this.handHeld();
  }

  /** Words in a user message Kilo wrote or started: whose they are, and when taken. */
  took(message: string): void {
    const say = this.byMessage.get(message);
    const continuation = this.continuations.has(message);
    if (this.turn && this.turn.first === undefined) {
      this.turn.first = message;
      this.turn.steerys = !!say || continuation;
    }
    if (say) {
      if (!say.takenAt) say.takenAt = Date.now();
      if (this.turn) for (const id of [say.id, ...say.also]) if (!this.turn.says.includes(id)) this.turn.says.push(id);
      for (const wake of say.wake.splice(0)) wake();
      this.asked = false;
      this.waiting = false;
    } else if (continuation) {
      this.asked = false;
      this.waiting = false;
    } else {
      // The person's own words: a park and a continuation hold until they speak.
      this.parked = false;
      this.continued = false;
      this.asked = false;
      this.waiting = false;
    }
  }

  async event(type: string, props: Fields, session: string): Promise<void> {
    const own = session === this.id;
    switch (type) {
      case "session.status": {
        if (!own) break;
        const status = fields(props.status);
        const was = this.status;
        this.status = text(status.type) ?? "idle";
        const error = text(status.message);
        this.retry =
          this.status === "retry"
            ? { attempt: count(status.attempt) ?? 1, ...(error ? { error: error.slice(0, ERROR_MAX) } : {}) }
            : undefined;
        if (this.status !== "idle") this.began();
        else if (was !== "idle" || this.turn) this.endedTurn();
        break;
      }
      case "session.idle":
        if (own && this.turn) {
          this.status = "idle";
          this.endedTurn();
        }
        break;
      case "session.updated":
        if (own) this.compacting = !!count(fields(fields(props.info).time).compacting);
        break;
      case "session.compacted":
        if (own) this.compacting = false;
        break;
      case "session.queue.changed": {
        if (!own) break;
        // Kilo's queue lists the user messages that wait for the running turn to yield; the
        // turn takes one when an assistant message answers it (`message.updated`), and an
        // abort empties the queue without taking any.
        const queue = Array.isArray(props.queued) ? props.queued.filter((message): message is string => typeof message === "string") : [];
        for (const message of queue) {
          const say = this.byMessage.get(message);
          if (say && !say.queued) {
            say.queued = true;
            for (const wake of say.wake.splice(0)) wake();
          }
        }
        this.queue = queue;
        break;
      }
      case "message.updated": {
        const info = infoOf(props.info);
        if (!own || !info) break;
        const known = this.messages.get(info.id);
        this.messages.set(info.id, { info, parts: known?.parts ?? new Map() });
        if (info.role === "assistant") {
          // Kilo answers the user message that its turn runs on: a new parent is words taken.
          if (info.parentID && info.parentID !== this.parent) {
            this.parent = info.parentID;
            this.took(info.parentID);
          }
          if (!this.lives.has(info.id) && !info.completed) {
            this.lives.set(info.id, { first: Date.now(), at: Date.now(), sent: {}, ended: false });
          }
          if (info.completed) {
            const live = this.lives.get(info.id);
            if (live) {
              live.at = info.completed;
              this.flushLive(info.id);
            }
            void this.refreshLeft();
            this.maybeSettle(info);
          }
        }
        break;
      }
      case "message.part.updated": {
        const part = partOf(props.part);
        if (part) this.part(part, session);
        return;
      }
      case "message.part.delta": {
        const messageID = text(props.messageID);
        const partID = text(props.partID);
        if (!own || props.field !== "text" || !messageID || !partID) break;
        const part = this.messages.get(messageID)?.parts.get(partID);
        if (!part) break;
        part.text += text(props.delta) ?? "";
        const live = this.lives.get(messageID);
        if (live) {
          live.at = Date.now();
          this.noteLive(messageID);
        }
        return;
      }
      case "permission.asked":
      case "question.asked": {
        const id = text(props.id);
        if (!id) break;
        const question = fields(Array.isArray(props.questions) ? props.questions[0] : undefined);
        const patterns = Array.isArray(props.patterns) ? props.patterns.filter((pattern) => typeof pattern === "string") : [];
        const title =
          type === "question.asked"
            ? (text(question.question) ?? text(question.header) ?? "A question")
            : `Allow ${text(props.permission) ?? "a tool"}${patterns.length ? `: ${patterns.join(", ")}` : ""}`;
        this.dialogs.set(id, { kind: type === "question.asked" ? "select" : "confirm", title: title.slice(0, 200), since: Date.now() });
        break;
      }
      case "permission.replied":
      case "question.replied":
      case "question.rejected": {
        const id = text(props.requestID) ?? text(props.permissionID);
        if (id) this.dialogs.delete(id);
        break;
      }
      case "session.deleted":
        if (own) {
          this.shut();
          process_.doors.delete(this.id);
        }
        return;
      default:
        return;
    }
    this.noteState();
  }

  part(part: PartOf, session: string): void {
    if (session !== this.id) {
      // A sub-agent's call runs inside the lead's `task` call that started its session.
      if (part.type !== "tool") return;
      const calls = this.childCalls.get(session) ?? new Map<string, string>();
      if (part.status === "running" || part.status === "pending") calls.set(part.callID, part.tool);
      else calls.delete(part.callID);
      this.childCalls.set(session, calls);
      this.noteState();
      return;
    }
    const message = this.messages.get(part.messageID);
    if (part.type === "text" || part.type === "reasoning") {
      if (!message) return;
      message.parts.set(part.id, { type: part.type, text: part.text });
      const live = this.lives.get(part.messageID);
      if (live) {
        live.at = Date.now();
        this.noteLive(part.messageID);
      }
      return;
    }
    if (part.type !== "tool") return;
    if (message) {
      if (part.status === "pending") message.parts.set(part.id, { type: "tool-pending", text: part.tool });
      else message.parts.delete(part.id);
    }
    if (part.status === "running") {
      const child = part.child;
      void spelled(this.client, part.tool).then((tool) => {
        const call = this.calls.get(part.callID);
        if (call) {
          if (child) call.child = child;
        } else if (!this.ended.some((ended) => ended.id === part.callID)) {
          this.calls.set(part.callID, { tool, since: part.start ?? Date.now(), ...(child ? { child } : {}) });
        }
        this.noteState();
      });
    } else if (part.status === "completed" || part.status === "error") {
      const call = this.calls.get(part.callID);
      this.calls.delete(part.callID);
      if (call?.child) this.childCalls.delete(call.child);
      if (call || !this.ended.some((ended) => ended.id === part.callID)) {
        this.ended.push({ id: part.callID, at: part.end ?? Date.now() });
        while (this.ended.length > ENDED_KEPT) this.ended.shift();
      }
    }
    this.noteState();
  }

  /** A Steery call came back: whether the turn asked, waits, or was parked. */
  steeryCall(tool: string, args: Fields, output: string): void {
    const command = tool === "bash" ? ran(args.command) : undefined;
    const inner = tool.startsWith(`${STEERY}_`) ? tool.slice(STEERY.length + 1) : undefined;
    if (tool === "question") this.asked = true;
    if (!command && inner === undefined) return;
    const op = command?.op ?? inner!.replace("_", ".");
    if (op === "requests.create" || op === "requests.get") this.asked = true;
    this.waiting = waits(command ?? { op, words: JSON.stringify(args) }, output);
    if (inner === "sessions_describe" && fields(args.park) === args.park && !/"error"|refus/i.test(output.slice(0, 200))) {
      this.parked = true;
    }
  }

  // ---- settle ---------------------------------------------------------------------

  /**
   * The last message of a turn finished on its own: ask the runtime whether Steery's
   * words keep the turn going, wait at most 1 s, and continue with them. A one-shot copy
   * asks nothing: Kilo has no hook between a turn's last answer and its end, and `kilo run`
   * (or a driven `kilo acp`) exits as the turn ends, before any words could go in.
   */
  maybeSettle(info: Info): void {
    if (!this.welcomed || this.lead || this.ending || this.oneShot()) return;
    if (info.error || info.finish === "tool-calls" || info.finish === "unknown" || !info.finish) return;
    if (this.settledMessages.has(info.id) || this.queue.length || this.held.length) return;
    this.settledMessages.add(info.id);
    const id = `settle-${info.id}`;
    const continued = this.continued;
    void (async () => {
      const { promise, resolve } = Promise.withResolvers<unknown>();
      const timer = setTimeout(() => resolve(undefined), SETTLE_WAIT_MS);
      this.settling.set(id, resolve);
      this.send({ t: "settle", id, asked: this.asked, parked: this.parked, waiting: this.waiting, continued });
      const ok = await promise;
      clearTimeout(timer);
      this.settling.delete(id);
      const say = text(fields(ok).say);
      if (!say?.trim()) return;
      this.continued = true;
      const message = mint("msg");
      this.continuations.add(message);
      await this.prompt(message, reminder(say));
    })().catch(() => {});
  }

  // ---- the runtime's asks ---------------------------------------------------------

  async prompt(message: string, words: string): Promise<void> {
    const picked = this.picked;
    this.picked = undefined;
    const result = await this.client.session.promptAsync({
      path: { id: this.id },
      body: {
        messageID: message,
        parts: [{ type: "text", text: words }],
        ...(picked ? { model: { providerID: picked.providerID, modelID: picked.modelID } } : {}),
        ...(picked?.variant ? { variant: picked.variant } : {}),
      },
    });
    if (result.error) throw new Error(failureOf(result.error)?.line ?? "Kilo refused the prompt");
  }

  async ask(frame: Fields): Promise<Answer> {
    if (this.ending) return { refused: "ending" };
    switch (frame.op) {
      case "say":
        return this.say(frame);
      case "pick":
        return this.pick(frame);
      case "stop":
        return this.stop(frame.say);
      case "force":
        return this.force(frame.say);
      case "compact":
        return this.compact();
      default:
        return { refused: "refused", error: `this copy does not know ${String(frame.op)}` };
    }
  }

  answerOf(say: Say): Answer | undefined {
    if (say.modelMissing) return { refused: "model-missing", error: say.modelMissing };
    if (say.takenAt) return { ok: { queued_at: say.queuedAt ?? say.takenAt, taken_at: say.takenAt } };
    if (say.letGo) return { refused: "not-taken", error: "Kilo let the words go with its turn" };
    return undefined;
  }

  /** Wait for the say to be taken, or (for steered words) held, at most 8 s to be held. */
  async settledSay(say: Say, held: boolean): Promise<Answer> {
    const deadline = Date.now() + HELD_WITHIN_MS;
    for (;;) {
      const answer = this.answerOf(say);
      if (answer) return answer;
      if (held && say.queued && say.queuedAt) return { ok: { queued_at: say.queuedAt } };
      if (this.ending) return { refused: "ending" };
      const seen = say.queued || say.takenAt;
      const left = seen ? 60_000 : deadline - Date.now();
      if (left <= 0) return { refused: "no-answer", error: "Kilo did not show the words held within 8 s" };
      await new Promise<void>((resolve) => {
        const timer = setTimeout(resolve, left);
        say.wake.push(() => {
          clearTimeout(timer);
          resolve();
        });
      });
    }
  }

  async say(frame: Fields): Promise<Answer> {
    if (this.lead) return { refused: "refused", error: "a sub-agent's conversation takes words only at its lead" };
    const id = text(frame.id) ?? "";
    const words = text(frame.words);
    const known = this.says.get(id);
    if (known) return this.answerOf(known) ?? this.settledSay(known, false);
    if (typeof words !== "string" || !words) return { refused: "refused", error: "no words, and this copy does not remember that say" };
    const mode = frame.mode === "queue" ? "queue" : frame.mode === "steer" ? "steer" : undefined;
    if (!mode) return { refused: "refused", error: "mode must be steer or queue" };
    if (frame.idle === true && !this.idle()) return { refused: "harness-busy", error: "a turn runs or words wait" };
    if (this.oneShot() && this.turns === 0 && !this.turn) {
      return { refused: "refused", error: "a one-shot run takes no words before its own turn" };
    }
    if (this.oneShot() && !this.busy()) return { refused: "ending", error: "this one-shot run's turn has ended" };
    const thrown = text(frame.throw);
    const first = thrown ? this.throws.get(thrown) : undefined;
    if (first) {
      if (!first.also.includes(id)) first.also.push(id);
      this.says.set(id, first);
      if (this.turn && first.takenAt && this.turn.says.includes(first.id)) this.turn.says.push(id);
      return this.answerOf(first) ?? this.settledSay(first, first.steered);
    }
    const say: Say = { id, also: [], words, mode, steered: false, queued: false, letGo: false, handedAgain: false, wake: [] };
    if (thrown) {
      say.thrown = thrown;
      this.throws.set(thrown, say);
    }
    this.says.set(id, say);
    if (mode === "queue" && this.busy()) {
      this.held.push(say);
      this.noteState();
      return this.settledSay(say, false).then((answer) => answer);
    }
    await this.hand(say);
    this.noteState();
    return this.settledSay(say, say.steered);
  }

  /** Hand the words to Kilo: into the running turn's queue, or to start one. */
  async hand(say: Say): Promise<void> {
    say.message = mint("msg");
    say.steered = this.busy();
    say.queued = false;
    this.byMessage.set(say.message, say);
    try {
      await this.prompt(say.message, say.words);
      say.queuedAt = Date.now();
    } catch (error) {
      say.modelMissing = String(error instanceof Error ? error.message : error);
      for (const wake of say.wake.splice(0)) wake();
    }
  }

  /** Words held for the end of a turn go in once Kilo is idle. */
  async handHeld(): Promise<void> {
    const say = this.held.shift();
    if (!say || this.busy()) {
      if (say) this.held.unshift(say);
      return;
    }
    await this.hand(say);
    this.noteState();
  }

  async pick(frame: Fields): Promise<Answer> {
    if (this.lead) return { refused: "refused", error: "a sub-agent's conversation takes a pick only at its lead" };
    const now = this.picked ?? this.model();
    let model: Model | undefined = now ? { ...now } : undefined;
    const asked = text(frame.model);
    if (asked) {
      const slash = asked.indexOf("/");
      const found = slash > 0 ? (await models(this.client)).get(asked) : undefined;
      if (!found) return { refused: "model-missing", error: `Kilo has no model ${asked}` };
      model = { providerID: asked.slice(0, slash), modelID: asked.slice(slash + 1), ...(model?.variant ? { variant: model.variant } : {}) };
    }
    const thinking = text(frame.thinking);
    if (thinking) {
      if (!model) return { refused: "selection-unsupported", error: "no model to set thinking on" };
      const variants = (await models(this.client)).get(`${model.providerID}/${model.modelID}`)?.variants ?? [];
      if (!variants.includes(thinking)) {
        return { refused: "selection-unsupported", error: `${model.providerID}/${model.modelID} has no variant ${thinking}` };
      }
      model.variant = thinking;
    } else if (asked) {
      delete model?.variant;
    }
    if (!model) return { ok: {} };
    this.picked = model;
    return { ok: { model: `${model.providerID}/${model.modelID}`, ...(model.variant ? { thinking: model.variant } : {}) } };
  }

  async abort(): Promise<boolean> {
    await this.client.session.abort({ path: { id: this.id } });
    const deadline = Date.now() + STOPPED_WITHIN_MS;
    while (this.busy() && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 25));
    return !this.busy();
  }

  async stop(sayId: unknown): Promise<Answer> {
    if (this.lead) return { refused: "refused", error: "a sub-agent's conversation stops only at its lead" };
    if (sayId !== undefined && (typeof sayId !== "string" || !sayId)) {
      return { refused: "refused", error: "a stop names the say whose turn it stops, or no say" };
    }
    // The person's pause from Steery parks the conversation whether or not a turn runs.
    if (sayId === undefined) this.parked = true;
    if (!this.busy()) return { ok: { stopped: false, running: [] } };
    if (typeof sayId === "string" && !this.turn?.says.includes(sayId)) {
      return { refused: "other-turn", error: "the turn that runs is not one its words began" };
    }
    if (!(await this.abort())) return { refused: "no-answer", error: "a turn still runs after 2.5 s" };
    return { ok: { stopped: true, running: [] } };
  }

  async force(sayId: unknown): Promise<Answer> {
    if (this.lead) return { refused: "refused", error: "a sub-agent's conversation takes this only at its lead" };
    const say = typeof sayId === "string" ? this.says.get(sayId) : undefined;
    if (!say || say.letGo) return { refused: "refused", error: "no words of that say wait here" };
    if (say.takenAt || !this.busy()) return { ok: { stopped: false, running: [] } };
    const held = this.held.indexOf(say);
    if (!(await this.abort())) return { refused: "no-answer", error: "a turn still runs after 2.5 s" };
    // Kilo drops its queue with an abort; the words go in again, once.
    if (held >= 0) this.held.splice(held, 1);
    if (!say.takenAt && !say.handedAgain) {
      say.handedAgain = true;
      say.letGo = false;
      if (say.message) this.byMessage.delete(say.message);
      await this.hand(say);
    }
    return { ok: { stopped: true, running: [] } };
  }

  /**
   * Kilo cannot put a given summary in place at a given cut without running its own
   * model, so it declines: the context starts where it did.
   */
  async compact(): Promise<Answer> {
    if (this.lead) return { refused: "refused", error: "a sub-agent's conversation is compacted at its lead" };
    if (this.busy()) return { refused: "harness-busy", error: "a turn runs" };
    const reply = await this.client.session.messages({ path: { id: this.id } });
    const infos = (reply.data ?? []).flatMap((message) => infoOf(message.info) ?? []);
    const summary = infos.findLastIndex((info) => info.role === "assistant" && info.summary);
    const from = infos[summary + 1]?.created ?? infos[0]?.created ?? Date.now();
    return { ok: { from, forgot: 0 } };
  }
}

/** Kilo's MCP servers by name, longest first, Steery's among them. */
function servers(client: Client): Promise<string[]> {
  return (process_.servers ??= (async () => {
    try {
      const reply = await client.config.get();
      const names = Object.keys(reply.data?.mcp ?? {});
      if (!names.includes(STEERY)) names.push(STEERY);
      return names.sort((a, b) => b.length - a.length);
    } catch {
      return [STEERY];
    }
  })());
}

/** Kilo's models by `<provider>/<model>`. */
function models(client: Client): Promise<Map<string, Listed>> {
  return (process_.models ??= (async () => {
    const found = new Map<string, Listed>();
    try {
      const reply = await client.config.providers();
      for (const provider of reply.data?.providers ?? []) {
        for (const [id, model] of Object.entries(provider.models ?? {})) {
          found.set(`${provider.id}/${id}`, {
            context: count(fields(fields(model).limit).context),
            variants: Object.keys(fields(fields(model).variants)),
          });
        }
      }
    } catch {
      process_.models = undefined;
    }
    return found;
  })());
}

/** A tool as the driver spells a call: `mcp__<server>__<tool>` for an MCP server's. */
async function spelled(client: Client, tool: string): Promise<string> {
  for (const server of await servers(client)) {
    if (tool.startsWith(`${server}_`)) return `mcp__${server}__${tool.slice(server.length + 1)}`;
  }
  return tool;
}

/** The root conversation a session belongs to, Kilo's sub-agents under their lead. */
function rootOf(client: Client, session: string): Promise<string | undefined> {
  let found = process_.parents.get(session);
  if (!found) {
    found = (async () => {
      try {
        const reply = await client.session.get({ path: { id: session } });
        const parent = reply.data?.parentID;
        if (typeof parent !== "string" || !parent) return session;
        return (await rootOf(client, parent)) ?? parent;
      } catch {
        process_.parents.delete(session);
        return undefined;
      }
    })();
    process_.parents.set(session, found);
  }
  return found;
}

function sessionOf(type: string, props: Fields): string | undefined {
  if (type === "message.part.updated") return text(fields(props.part).sessionID);
  if (type === "message.updated") return text(fields(props.info).sessionID);
  if (type === "session.updated" || type === "session.deleted" || type === "session.created") {
    return text(fields(props.info).id) ?? text(props.sessionID);
  }
  return text(props.sessionID);
}

export const Steery: Plugin = async (input) => {
  const started = startedBy();
  if (started === "nothing") return {};
  const mode = modeOf(started);
  const lead = (() => {
    const value = process.env.STEERY_LEAD;
    const at = value?.indexOf(":") ?? -1;
    return value && at > 0 ? { harness: value.slice(0, at), external_id: value.slice(at + 1) } : undefined;
  })();
  const client = input.client;
  let line = Promise.resolve();

  /** The door of a root conversation, opened the first time it comes alive here. */
  const doorOf = (root: string): Door => {
    let door = process_.doors.get(root);
    if (!door) {
      // A process that inherited a lead naming another conversation works for it.
      const theirs = lead && !(lead.harness === HARNESS && lead.external_id === root) ? lead : undefined;
      door = new Door(root, client, mode, theirs, input.directory);
      process_.doors.set(root, door);
      door.dial();
    }
    return door;
  };

  const hooks: Hooks = {
    event: async ({ event }) => {
      const { type, properties } = event as { type: string; properties?: unknown };
      const props = fields(properties);
      const session = sessionOf(type, props);
      if (!session) return;
      // Kilo does not wait for this hook: events go through one line, in the order they came.
      line = line
        .then(async () => {
          const root = await rootOf(client, session);
          if (!root) return;
          if (type === "session.deleted" && root !== session) return;
          if (type === "session.deleted" && !process_.doors.has(root)) return;
          await doorOf(root).event(type, props, session);
        })
        .catch(() => {});
    },
    "chat.message": async (_input, output) => {
      try {
        const root = await rootOf(client, output.message.sessionID);
        const door = root ? process_.doors.get(root) : undefined;
        // A pick goes to the next prompt, the person's own or Steery's.
        if (!door?.picked || root !== output.message.sessionID) return;
        output.message.model = { ...door.picked };
        door.picked = undefined;
      } catch {
        // The prompt goes on as it was.
      }
    },
    "shell.env": async (env, output) => {
      try {
        if (!env.sessionID) return;
        const root = await rootOf(client, env.sessionID);
        if (root) output.env.STEERY_LEAD = lead && !process_.doors.get(root) ? `${lead.harness}:${lead.external_id}` : `${HARNESS}:${root}`;
      } catch {
        // The command runs as it would.
      }
    },
    "tool.execute.after": async (call, output) => {
      try {
        const root = await rootOf(client, call.sessionID);
        if (root !== call.sessionID) return;
        process_.doors.get(root)?.steeryCall(call.tool, fields(call.args), String(output?.output ?? ""));
      } catch {
        // Nothing to note.
      }
    },
  };
  return hooks;
};
