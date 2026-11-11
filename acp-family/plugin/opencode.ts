/**
 * Steery's plugin for OpenCode 2: the door of PROTOCOL.md §4.4, version 1. OpenCode loads it
 * in every process that runs its server (the background service, and the `serve --stdio`
 * child of a standalone `opencode run` or `opencode acp`), and for each root conversation
 * that comes alive there it dials the runtime's loopback door, says what the conversation
 * is doing (`state`, `live`), and answers the runtime's asks through the plugin context.
 *
 * OpenCode keeps its record in its SQLite file, which the driver reads, so this door
 * carries no entries. OpenCode's own sub-agents open no door: their calls show as the
 * children of the lead's call that started them.
 */
import type { Plugin } from "@opencode/plugin";
import { spawnSync } from "node:child_process";
import { readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

type Context = Plugin.Context;

/** The runtime's door, filled in by the driver's `setup/plan`. */
const DOOR = "__STEERY_DOOR__";
const HARNESS = "opencode";
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
const RESUBSCRIBE_MS = 1000;

/** Steery's MCP server. OpenCode runs an MCP tool inside its `execute` tool, as `<server>.<tool>`. */
const STEERY = "steery";

type Json = Record<string, unknown>;
type Fields = { readonly [key: string]: unknown };
type Mode = "tui" | "rpc" | "json" | "print";
type Answer = { ok: unknown } | { refused: string; error?: string };
/** A model as OpenCode refers to one: `id` is the model's own id. */
type Model = { providerID: string; id: string; variant?: string };

function fields(value: unknown): Fields {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Fields) : {};
}

const text = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined);
const count = (value: unknown): number | undefined => (typeof value === "number" && Number.isFinite(value) ? value : undefined);

/** The parent pid and command of a process, by `ps`. */
function processOf(pid: number): { ppid: number; command: string } | undefined {
  try {
    const ps = spawnSync("ps", ["-o", "ppid=,command=", "-p", String(pid)], { encoding: "utf8" });
    const [, ppid, command] = /^\s*(\d+)\s+(.*)$/.exec(ps.stdout.trim()) ?? [];
    return ppid ? { ppid: Number(ppid), command: command ?? "" } : undefined;
  } catch {
    return undefined;
  }
}

/** The OpenCode command a process runs: `run`, `acp`, `serve`, or none. */
function commandOf(argv: string[]): string | undefined {
  // The binary is a Bun executable: argv is `bun /$bunfs/root/opencode <command> …`.
  const at = argv.findIndex((arg) => /(^|\/)opencode$/.test(arg));
  return argv.slice(at + 1).find((arg) => !arg.startsWith("-"));
}

const parent = processOf(process.ppid);
const parentCommand = parent ? commandOf(parent.command.split(/\s+/)) : undefined;

/**
 * `STEERY_STARTED=<pid>:<what>`, trusted where this process's parent is that pid. A
 * standalone `opencode run` or `opencode acp` runs its server as a `serve --stdio` child,
 * which loads this plugin, so where the parent is that command, its own parent counts.
 */
function startedBy(): string | undefined {
  const value = process.env.STEERY_STARTED;
  const at = value?.indexOf(":") ?? -1;
  if (!value || at < 1) return undefined;
  const pid = Number(value.slice(0, at));
  if (pid <= 1) return undefined;
  if (pid === process.ppid) return value.slice(at + 1);
  const standalone = parentCommand === "run" || parentCommand === "acp";
  return standalone && parent?.ppid === pid ? value.slice(at + 1) : undefined;
}

/**
 * Who is at this process, from how it was started. A turn this driver started runs
 * `opencode acp` for that turn alone. A `serve --stdio` child belongs to its parent: under
 * `opencode run` a one-shot run, under `opencode acp` a program. Any other server is the
 * background service, which the TUI and every attached `opencode run` share.
 */
function modeOf(started: string | undefined): Mode {
  if (started?.startsWith("turn:") || started?.startsWith("turn@")) return "json";
  const args = process.argv.slice(2);
  if (!args.includes("--stdio")) return "tui";
  switch (parentCommand) {
    case "run": {
      const words = parent!.command;
      return /--format[=\s]+json\b/.test(words) ? "json" : "print";
    }
    case "acp":
      return "rpc";
    default:
      return "tui";
  }
}

/** An OpenCode id of its own kind: `<prefix>_` and an ascending time, as OpenCode mints them. */
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

function modelOf(value: unknown): Model | undefined {
  const model = fields(value);
  const providerID = text(model.providerID);
  const id = text(model.id);
  if (!providerID || !id) return undefined;
  const variant = text(model.variant);
  // OpenCode names a model's own setting `default`: no thinking level was picked.
  return { providerID, id, ...(variant && variant !== "default" ? { variant } : {}) };
}

function errorLine(value: unknown): string | undefined {
  const error = fields(value);
  const line = text(error.message) || text(fields(error.data).message) || text(error.type) || text(error.name);
  return line?.slice(0, ERROR_MAX);
}

/** The tokens a response used, its whole context. */
function usedOf(value: unknown): number | undefined {
  const tokens = fields(value);
  const cache = fields(tokens.cache);
  const sum = [tokens.input, tokens.output, tokens.reasoning, cache.read, cache.write].reduce<number>(
    (all, part) => all + (count(part) ?? 0),
    0,
  );
  return sum || undefined;
}

/** The text of a tool result's content. */
function contentText(value: unknown): string {
  if (!Array.isArray(value)) return "";
  return value.map((part) => text(fields(part).text) ?? "").join("\n");
}

/** One assistant message: a step of a turn. */
type Message = {
  id: string;
  model?: Model;
  /** Its place in the order of messages and selections this door saw. */
  born: number;
  completed?: number;
  finish?: string;
  error?: string;
  aborted?: boolean;
  used?: number;
  /** Text and reasoning by `<ordinal>:<type>`; a forming call as `tool-pending`. */
  parts: Map<string, { type: string; text: string }>;
};

/** How a message stopped, as the door's `live.end.stop` names it. */
function stopOf(message: Message): string {
  if (message.aborted) return "aborted";
  if (message.error) return "error";
  if (message.finish === "length") return "length";
  if (message.finish === "tool-calls") return "tool_use";
  return "stop";
}

/** Words Steery says into the conversation, marked as Steery's. */
const reminder = (words: string) => `<system-reminder reason="steery">\n${words}\n</system-reminder>`;

/** Words of a `say`, from the ask until OpenCode takes them or lets them go. */
type Say = {
  id: string;
  also: string[];
  words: string;
  mode: "steer" | "queue";
  thrown?: string;
  /** OpenCode's id for the inbox item, and the user message, that carries the words. */
  message?: string;
  queuedAt?: number;
  takenAt?: number;
  /** Handed while a turn ran: OpenCode holds them until the turn's next step. */
  steered: boolean;
  /** In OpenCode's inbox, not yet delivered. */
  queued: boolean;
  letGo: boolean;
  modelMissing?: string;
  handedAgain: boolean;
  wake: Array<() => void>;
};

/** A model as OpenCode lists it: its context window and the variants (thinking levels) it has. */
type Listed = { context?: number; variants: string[] };

type Live = { first: number; at: number; sent: Record<string, string[]>; timer?: ReturnType<typeof setTimeout>; ended: boolean };

/** What the conversations of this process share. */
type Shared = {
  models?: Promise<Map<string, Listed>>;
  parents: Map<string, Promise<string | undefined>>;
  doors: Map<string, Door>;
};

const process_: Shared = (() => {
  const key = Symbol.for("steery.opencode");
  const global = globalThis as unknown as Record<symbol, Shared | undefined>;
  return (global[key] ??= { parents: new Map(), doors: new Map() });
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

  running = false;
  /** How many executions ended, so an interrupt knows its own ended. */
  endedTurns = 0;
  retry: Json | undefined;
  compacting = false;
  turn: { began_at: number; says: string[]; steerys: boolean; first?: string } | undefined;
  turns = 0;
  /** The model the session has selected, and its place in the order `born` counts. */
  selected: Model | undefined;
  selectedBorn = 0;
  births = 0;
  held: Say[] = [];
  says = new Map<string, Say>();
  byMessage = new Map<string, Say>();
  throws = new Map<string, Say>();
  answers = new Map<string, Promise<Answer>>();
  messages = new Map<string, Message>();
  calls = new Map<string, { tool: string; since: number; child?: string }>();
  forming = new Map<string, string>();
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
  /** Who waits for the next step to end: the idle hook runs before its `step.ended` arrives. */
  stepWaiters: (() => void)[] = [];
  sentState = "";
  stateTimer: ReturnType<typeof setTimeout> | undefined;
  stateAt = 0;
  left: number | undefined;

  constructor(
    readonly id: string,
    readonly ctx: Context,
    readonly mode: Mode,
    readonly lead: { harness: string; external_id: string } | undefined,
    readonly cwd: string,
  ) {}

  oneShot(): boolean {
    return this.mode === "json" || this.mode === "print";
  }

  busy(): boolean {
    return this.running || this.turn !== undefined;
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
        this.send({ t: "lost", why: "OpenCode keeps its record in its SQLite file, which the driver reads; this door carries no entries" });
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
      void this.answer(id, frame).then((said) => this.send({ t: "answer", id, ...said }));
    }
  }

  /** The answer to an ask, the first one again when the same `id` comes again. */
  answer(id: string, frame: Fields): Promise<Answer> {
    let answer = this.answers.get(id);
    if (!answer) {
      answer = this.ask(frame).catch((error: unknown): Answer => ({ refused: "refused", error: String(error) }));
      this.answers.set(id, answer);
      while (this.answers.size > ANSWERS_KEPT) this.answers.delete(this.answers.keys().next().value as string);
      // Only `no-answer` and answers that took are kept; any other refusal is forgotten.
      void answer.then((said) => {
        if ("refused" in said && said.refused !== "no-answer") this.answers.delete(id);
      });
    }
    return answer;
  }

  // ---- state ----------------------------------------------------------------------

  /** The newest of: the model of the newest step, and the session's selection. */
  model(): Model | undefined {
    const newest = [...this.messages.values()].reverse().find((message) => message.model);
    if (!newest?.model || (this.selected && this.selectedBorn > newest.born)) return this.selected ?? newest?.model;
    return newest.model;
  }

  select(model: Model): void {
    this.selected = model;
    this.selectedBorn = ++this.births;
  }

  async refreshLeft(): Promise<void> {
    try {
      const model = this.model();
      const used = [...this.messages.values()].reverse().find((message) => message.used)?.used;
      const limit = model ? (await models(this.ctx)).get(`${model.providerID}/${model.id}`)?.context : undefined;
      this.left = used && limit && limit > 0 ? Math.max(0, Math.min(1, (limit - used) / limit)) : undefined;
    } catch {
      this.left = undefined;
    }
    this.noteState();
  }

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
      retrying: !!this.retry,
      ...(this.retry ? { retry: this.retry } : {}),
      ...(model ? { model: `${model.providerID}/${model.id}` } : {}),
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
    const done = !!message.completed && !cut;
    const forming = [...message.parts.values()].find((part) => part.type === "tool-pending")?.text;
    if (!parts.length && !done) return;
    this.send({
      t: "live",
      message: id,
      at: live.at,
      first_at: live.first,
      parts,
      ...(forming ? { forming } : {}),
      ...(done ? { end: { stop: stopOf(message) } } : {}),
    });
    if (done) {
      live.ended = true;
      this.lives.delete(id);
    } else if (cut) this.noteLive(id);
  }

  // ---- what OpenCode says ---------------------------------------------------------

  /** A turn begins: an execution started. */
  began(): void {
    this.running = true;
    if (this.turn) return;
    this.turn = { began_at: Date.now(), says: [], steerys: false };
    this.turns++;
  }

  /** A turn ends: its execution succeeded, failed or was interrupted. */
  endedTurn(error: string | undefined, interrupted: boolean): void {
    const turn = this.turn;
    this.turn = undefined;
    this.running = false;
    this.endedTurns++;
    this.retry = undefined;
    if (interrupted) {
      for (const message of this.messages.values()) {
        if (message.completed) continue;
        message.completed = Date.now();
        message.aborted = true;
        this.flushLive(message.id);
      }
    }
    if (turn?.steerys) {
      const failure = interrupted ? undefined : error;
      this.settled.push({
        began_at: turn.began_at,
        at: Date.now(),
        says: turn.says,
        failed: !!failure,
        ...(failure ? { error: failure } : {}),
      });
      while (this.settled.length > SETTLED_KEPT) this.settled.shift();
    }
    for (const [id] of this.calls) this.endCall(id, Date.now());
    void this.handHeld();
  }

  /** Words OpenCode delivered from its inbox into the conversation: whose they are, and when taken. */
  took(message: string): void {
    const say = this.byMessage.get(message);
    const continuation = this.continuations.has(message);
    if (!this.turn) this.began();
    if (this.turn && this.turn.first === undefined) {
      this.turn.first = message;
      this.turn.steerys = !!say || continuation;
    }
    if (say) {
      if (!say.takenAt) say.takenAt = Date.now();
      say.queued = false;
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

  messageOf(id: string): Message {
    let message = this.messages.get(id);
    if (!message) {
      message = { id, born: ++this.births, parts: new Map() };
      this.messages.set(id, message);
      this.lives.set(id, { first: Date.now(), at: Date.now(), sent: {}, ended: false });
    }
    return message;
  }

  textOf(data: Fields, type: string): { message: Message; key: string } | undefined {
    const id = text(data.assistantMessageID);
    if (!id) return undefined;
    const message = this.messageOf(id);
    const key = `${String(count(data.ordinal) ?? 0).padStart(6, "0")}:${type}`;
    if (!message.parts.has(key)) message.parts.set(key, { type, text: "" });
    return { message, key };
  }

  touchedLive(message: string): void {
    const live = this.lives.get(message);
    if (!live) return;
    live.at = Date.now();
    this.noteLive(message);
  }

  endCall(id: string, at: number): void {
    const call = this.calls.get(id);
    this.calls.delete(id);
    this.forming.delete(id);
    if (call?.child) this.childCalls.delete(call.child);
    if (call || !this.ended.some((ended) => ended.id === id)) {
      this.ended.push({ id, at });
      while (this.ended.length > ENDED_KEPT) this.ended.shift();
    }
  }

  event(type: string, data: Fields, session: string, created: number): void {
    if (session !== this.id) return this.childEvent(type, data, session);
    switch (type) {
      case "session.execution.started":
        this.began();
        break;
      case "session.execution.succeeded":
        this.endedTurn(undefined, false);
        break;
      case "session.execution.failed":
        this.endedTurn(errorLine(data.error), false);
        break;
      case "session.execution.interrupted":
        this.endedTurn(undefined, true);
        break;
      case "session.status": {
        const status = fields(data.status);
        if (status.type === "retry") {
          const error = text(status.message);
          this.retry = { attempt: count(status.attempt) ?? 1, ...(error ? { error: error.slice(0, ERROR_MAX) } : {}) };
        } else if (status.type === "busy") {
          this.retry = undefined;
          this.began();
        } else {
          this.retry = undefined;
        }
        break;
      }
      case "session.retry.scheduled": {
        const error = errorLine(data.error);
        this.retry = { attempt: count(data.attempt) ?? 1, ...(error ? { error } : {}) };
        break;
      }
      case "session.model.selected": {
        const model = modelOf(data.model);
        if (model) this.select(model);
        break;
      }
      case "session.compaction.started":
        this.compacting = true;
        break;
      case "session.compaction.ended":
      case "session.compaction.failed":
        this.compacting = false;
        break;
      case "session.inbox.enqueued": {
        const say = this.byMessage.get(text(data.inboxID) ?? "");
        if (say) {
          say.queuedAt ??= created;
          say.queued = true;
          for (const wake of say.wake.splice(0)) wake();
        }
        break;
      }
      case "session.inbox.delivered": {
        const message = text(data.inboxID);
        if (message) this.took(message);
        break;
      }
      case "session.inbox.cancelled": {
        const say = this.byMessage.get(text(data.inboxID) ?? "");
        if (say && !say.takenAt) {
          say.letGo = true;
          for (const wake of say.wake.splice(0)) wake();
        }
        break;
      }
      case "session.step.started": {
        this.retry = undefined;
        const message = this.messageOf(text(data.assistantMessageID) ?? "");
        message.model = modelOf(data.model) ?? message.model;
        break;
      }
      case "session.step.ended":
      case "session.step.failed": {
        const id = text(data.assistantMessageID);
        if (!id) break;
        const message = this.messageOf(id);
        message.completed = created;
        message.finish = text(data.finish);
        message.error = type === "session.step.failed" ? (errorLine(data.error) ?? "the step failed") : undefined;
        message.used = usedOf(data.tokens) ?? message.used;
        const live = this.lives.get(id);
        if (live) {
          live.at = Math.max(live.at, created);
          this.flushLive(id);
        }
        void this.refreshLeft();
        for (const wake of this.stepWaiters.splice(0)) wake();
        break;
      }
      case "session.text.started":
      case "session.reasoning.started":
        this.textOf(data, type === "session.text.started" ? "text" : "reasoning");
        return;
      case "session.text.delta":
      case "session.reasoning.delta": {
        const found = this.textOf(data, type === "session.text.delta" ? "text" : "reasoning");
        if (!found) return;
        found.message.parts.get(found.key)!.text += text(data.delta) ?? "";
        this.touchedLive(found.message.id);
        return;
      }
      case "session.text.ended":
      case "session.reasoning.ended": {
        const found = this.textOf(data, type === "session.text.ended" ? "text" : "reasoning");
        if (!found) return;
        found.message.parts.get(found.key)!.text = text(data.text) ?? found.message.parts.get(found.key)!.text;
        this.touchedLive(found.message.id);
        return;
      }
      case "session.tool.input.started": {
        const id = text(data.id);
        const message = text(data.assistantMessageID);
        const tool = text(data.name) ?? "";
        if (!id) break;
        this.forming.set(id, tool);
        if (message) {
          this.messageOf(message).parts.set(`tool:${id}`, { type: "tool-pending", text: tool });
          this.touchedLive(message);
        }
        break;
      }
      case "session.tool.called": {
        const id = text(data.id);
        if (!id) break;
        const message = text(data.assistantMessageID);
        if (message) this.messages.get(message)?.parts.delete(`tool:${id}`);
        if (!this.calls.has(id) && !this.ended.some((ended) => ended.id === id)) {
          const tool = text(data.name) ?? this.forming.get(id) ?? "tool";
          this.calls.set(id, { tool, since: created });
        }
        this.forming.delete(id);
        break;
      }
      case "session.tool.success":
      case "session.tool.failed": {
        const id = text(data.id);
        if (!id) break;
        const message = text(data.assistantMessageID);
        if (message) this.messages.get(message)?.parts.delete(`tool:${id}`);
        const call = this.calls.get(id);
        if (type === "session.tool.success" && call) this.steeryCall(call.tool, data);
        this.endCall(id, created);
        break;
      }
      case "permission.asked": {
        const id = text(data.id);
        if (!id) break;
        const resources = Array.isArray(data.resources) ? data.resources.filter((one) => typeof one === "string") : [];
        const title = `Allow ${text(data.action) ?? "a tool"}${resources.length ? `: ${resources.join(", ")}` : ""}`;
        this.dialogs.set(id, { kind: "confirm", title: title.slice(0, 200), since: Date.now() });
        break;
      }
      case "form.created": {
        const form = fields(data.form);
        const id = text(form.id);
        if (!id) break;
        this.dialogs.set(id, { kind: "select", title: (text(form.title) ?? "A question").slice(0, 200), since: Date.now() });
        break;
      }
      case "permission.replied":
      case "form.replied":
      case "form.cancelled": {
        const id = text(data.requestID) ?? text(data.id);
        if (id) this.dialogs.delete(id);
        break;
      }
      case "session.deleted":
        this.shut();
        process_.doors.delete(this.id);
        return;
      default:
        return;
    }
    this.noteState();
  }

  /** A sub-agent's event: its calls run inside the lead's call that started its session. */
  childEvent(type: string, data: Fields, session: string): void {
    if (type === "session.created") {
      // The lead's newest running call without a child started this one.
      const call = [...this.calls.values()].reverse().find((call) => !call.child);
      if (call) call.child = session;
    } else if (type === "session.tool.called" || type === "session.tool.input.started") {
      const id = text(data.id);
      if (!id) return;
      const calls = this.childCalls.get(session) ?? new Map<string, string>();
      calls.set(id, text(data.name) ?? calls.get(id) ?? "tool");
      this.childCalls.set(session, calls);
    } else if (type === "session.tool.success" || type === "session.tool.failed") {
      const id = text(data.id);
      if (id) this.childCalls.get(session)?.delete(id);
    } else {
      return;
    }
    this.noteState();
  }

  /**
   * A call came back: whether the turn asked, waits, or was parked. OpenCode runs an MCP
   * tool inside `execute` and lists each one it ran in the result's `metadata.toolCalls`.
   */
  steeryCall(tool: string, data: Fields): void {
    const output = contentText(data.content);
    if (tool === "question") this.asked = true;
    const ran_: Array<{ op: string; args: Fields; command?: Ran }> = [];
    if (tool === "shell" || tool === "bash") {
      const command = ran(fields(data.input).command ?? fields(fields(data.metadata).input).command);
      if (command) ran_.push({ op: command.op, args: {}, command });
    }
    const inner = (name: string): string | undefined =>
      name.startsWith(`${STEERY}.`) || name.startsWith(`${STEERY}_`) ? name.slice(STEERY.length + 1) : undefined;
    const listed = Array.isArray(fields(data.metadata).toolCalls) ? (fields(data.metadata).toolCalls as unknown[]) : [];
    for (const call of [{ tool, input: data.input }, ...listed.map(fields)]) {
      const name = inner(text(call.tool) ?? "");
      if (name) ran_.push({ op: name.replace("_", "."), args: fields(call.input) });
    }
    for (const { op, args, command } of ran_) {
      if (op === "requests.create" || op === "requests.get") this.asked = true;
      this.waiting = waits(command ?? { op, words: JSON.stringify(args) }, output);
      if (op === "sessions.describe" && fields(args.park) === args.park && !/"error"|refus/i.test(output.slice(0, 200))) {
        this.parked = true;
      }
    }
  }

  // ---- settle ---------------------------------------------------------------------

  /**
   * The turn is about to go idle (OpenCode's `experimental.idle` hook, which waits for
   * this): ask the runtime whether Steery's words keep the turn going, wait at most 1 s,
   * and give them to the same execution. A one-shot copy asks nothing: a driven turn asks
   * Steery's `session/prompt` hook from the driver's turn instead.
   */
  async settle(background: number): Promise<void> {
    if (!this.welcomed || this.lead || this.ending || this.oneShot() || background) return;
    // OpenCode runs the hook as the model's stream ends, before it says the step ended.
    const newest = [...this.messages.values()].at(-1);
    if (newest && !newest.completed) {
      await new Promise<void>((resolve) => {
        const timer = setTimeout(resolve, SETTLE_WAIT_MS);
        this.stepWaiters.push(() => {
          clearTimeout(timer);
          resolve();
        });
      });
    }
    const last = [...this.messages.values()].reverse().find((message) => message.completed);
    if (!last || last.error || last.aborted || last.finish === "tool-calls" || last.finish === "unknown" || !last.finish) return;
    if (this.held.length || this.pending()) return;
    const id = `settle-${last.id}`;
    if (this.settling.has(id)) return;
    const continued = this.continued;
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
    await this.prompt(message, reminder(say), "steer");
  }

  // ---- the runtime's asks ---------------------------------------------------------

  async prompt(message: string, words: string, delivery: "steer" | "queue"): Promise<void> {
    await this.ctx.session.prompt({ sessionID: this.id, id: message, text: words, delivery });
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
    if (say.letGo) return { refused: "not-taken", error: "OpenCode let the words go" };
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
      if (left <= 0) return { refused: "no-answer", error: "OpenCode did not show the words held within 8 s" };
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
    // OpenCode's own `queue` delivery parks words for a next turn that `force` cannot
    // promote, so the plugin holds queued words itself and hands them when the turn ends.
    if (mode === "queue" && this.busy()) {
      this.held.push(say);
      this.noteState();
      return this.settledSay(say, false);
    }
    await this.hand(say);
    this.noteState();
    return this.settledSay(say, say.steered);
  }

  /** Hand the words to OpenCode: into the running turn at its next step, or to start one. */
  async hand(say: Say): Promise<void> {
    say.message ??= mint("msg");
    say.steered = this.busy();
    this.byMessage.set(say.message, say);
    try {
      await this.prompt(say.message, say.words, "steer");
      say.queuedAt ??= Date.now();
    } catch (error) {
      say.modelMissing = String(error instanceof Error ? error.message : error);
      for (const wake of say.wake.splice(0)) wake();
    }
  }

  /** Words held for the end of a turn go in once OpenCode is idle. */
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
    const now = this.model();
    let model: Model | undefined = now ? { ...now } : undefined;
    const asked = text(frame.model);
    const thinking = text(frame.thinking);
    if (!asked && !thinking) {
      // A pick of nothing reads the selection back.
      const read = modelOf((await this.ctx.session.get({ sessionID: this.id })).model) ?? model;
      return { ok: read ? { model: `${read.providerID}/${read.id}`, ...(read.variant ? { thinking: read.variant } : {}) } : {} };
    }
    if (asked) {
      const slash = asked.indexOf("/");
      const found = slash > 0 ? (await models(this.ctx)).get(asked) : undefined;
      if (!found) return { refused: "model-missing", error: `OpenCode has no model ${asked}` };
      model = { providerID: asked.slice(0, slash), id: asked.slice(slash + 1), ...(model?.variant ? { variant: model.variant } : {}) };
    }
    if (thinking) {
      if (!model) return { refused: "selection-unsupported", error: "no model to set thinking on" };
      const variants = (await models(this.ctx)).get(`${model.providerID}/${model.id}`)?.variants ?? [];
      if (!variants.includes(thinking)) {
        return { refused: "selection-unsupported", error: `${model.providerID}/${model.id} has no variant ${thinking}` };
      }
      model.variant = thinking;
    } else if (asked && model) {
      const variants = (await models(this.ctx)).get(`${model.providerID}/${model.id}`)?.variants ?? [];
      if (model.variant && !variants.includes(model.variant)) delete model.variant;
    }
    if (!model) return { ok: {} };
    // The session's selection is its model from the next step on; read it back.
    await this.ctx.session.switchModel({ sessionID: this.id, model });
    const read = modelOf((await this.ctx.session.get({ sessionID: this.id })).model) ?? model;
    this.select(read);
    this.noteState();
    return { ok: { model: `${read.providerID}/${read.id}`, ...(read.variant ? { thinking: read.variant } : {}) } };
  }

  /** Interrupt the execution that runs; true once it ended (held words may start the next). */
  async interrupt(resume: boolean): Promise<boolean> {
    const ended = this.endedTurns;
    await this.ctx.session.interrupt({ sessionID: this.id, resume });
    const deadline = Date.now() + STOPPED_WITHIN_MS;
    while (this.running && this.endedTurns === ended && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
    return !this.running || this.endedTurns !== ended;
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
    if (!(await this.interrupt(false))) return { refused: "no-answer", error: "a turn still runs after 2.5 s" };
    return { ok: { stopped: true, running: [] } };
  }

  async force(sayId: unknown): Promise<Answer> {
    if (this.lead) return { refused: "refused", error: "a sub-agent's conversation takes this only at its lead" };
    const say = typeof sayId === "string" ? this.says.get(sayId) : undefined;
    if (!say || say.letGo) return { refused: "refused", error: "no words of that say wait here" };
    if (say.takenAt || !this.busy()) return { ok: { stopped: false, running: [] } };
    const held = this.held.indexOf(say);
    // Out of the held words first, so the end of this turn does not hand them a second time.
    if (held >= 0) this.held.splice(held, 1);
    // Interrupted with `resume`, OpenCode starts the next turn on the steered words it holds.
    if (!(await this.interrupt(held < 0))) {
      if (held >= 0) this.held.splice(held, 0, say);
      return { refused: "no-answer", error: "a turn still runs after 2.5 s" };
    }
    if (held >= 0 && !say.handedAgain) {
      say.handedAgain = true;
      await this.hand(say);
    }
    return { ok: { stopped: true, running: [] } };
  }

  /**
   * OpenCode's `compaction` hook can put a given summary in place, but OpenCode keeps its
   * own recent tail and has no way to cut at a given entry, so this copy declines as
   * Kilo's does: the context starts where it did.
   */
  async compact(): Promise<Answer> {
    if (this.lead) return { refused: "refused", error: "a sub-agent's conversation is compacted at its lead" };
    if (this.busy()) return { refused: "harness-busy", error: "a turn runs" };
    const context = await this.ctx.session.context({ sessionID: this.id });
    const first = Array.isArray(context) ? fields(context[0]) : {};
    const from = count(fields(first.time).created) ?? Date.now();
    return { ok: { from, forgot: 0 } };
  }
}

/** OpenCode's models by `<provider>/<model>`. */
function models(ctx: Context): Promise<Map<string, Listed>> {
  return (process_.models ??= (async () => {
    const found = new Map<string, Listed>();
    try {
      const reply = await ctx.model.list({});
      for (const model of reply.data ?? []) {
        found.set(`${model.providerID}/${model.id}`, {
          context: count(model.limit?.context),
          variants: (model.variants ?? []).map((variant) => variant.id).filter((id) => id !== "default"),
        });
      }
    } catch {
      process_.models = undefined;
    }
    return found;
  })());
}

/** The root conversation a session belongs to, OpenCode's sub-agents under their lead. */
function rootOf(ctx: Context, session: string): Promise<string | undefined> {
  let found = process_.parents.get(session);
  if (!found) {
    found = (async () => {
      try {
        const parent = (await ctx.session.get({ sessionID: session })).parentID;
        if (typeof parent !== "string" || !parent) return session;
        return (await rootOf(ctx, parent)) ?? parent;
      } catch {
        process_.parents.delete(session);
        return undefined;
      }
    })();
    process_.parents.set(session, found);
  }
  return found;
}

function sessionOf(type: string, data: Fields): string | undefined {
  if (type === "form.created") return text(fields(data.form).sessionID);
  return text(data.sessionID);
}

/**
 * A `compact` ask a turn the runtime started carries in a file (`STEERY_COMPACT`), as the
 * door answers one, before the turn's prompt runs. The answer frame goes to
 * `compacted.json` beside the ask, readable by its owner only; an ask that cannot be read
 * is answered nothing. The variable is taken from `process.env` before any session runs,
 * so nothing this process starts inherits it.
 */
const compactFile = (() => {
  const file = process.env.STEERY_COMPACT;
  delete process.env.STEERY_COMPACT;
  return file || undefined;
})();

async function compactAsked(file: string, door: Door): Promise<void> {
  const taken = `${file}.taken`;
  let ask: Fields;
  try {
    renameSync(file, taken);
    ask = fields(JSON.parse(readFileSync(taken, "utf8")));
  } catch {
    return;
  } finally {
    rmSync(taken, { force: true });
  }
  const id = text(ask.id);
  if (ask.t !== "ask" || !id) return;
  const said: Answer =
    ask.op !== "compact"
      ? { refused: "refused", error: `this copy does not know ${String(ask.op)}` }
      : await door.compact().catch((error: unknown): Answer => ({ refused: "refused", error: String(error) }));
  try {
    writeFileSync(join(dirname(file), "compacted.json"), JSON.stringify({ t: "answer", id, ...said }), { mode: 0o600 });
  } catch {
    // The runtime reads no answer, and says the words as it would without one.
  }
}

const plugin: Plugin.Plugin = {
  id: "steery",
  setup: async (ctx) => {
    const started = startedBy();
    if (started === "nothing") return;
    const mode = modeOf(started);
    const lead = (() => {
      const value = process.env.STEERY_LEAD;
      const at = value?.indexOf(":") ?? -1;
      return value && at > 0 ? { harness: value.slice(0, at), external_id: value.slice(at + 1) } : undefined;
    })();
    const cwd = text(fields(ctx.location).directory) ?? process.cwd();
    let compact = compactFile;
    let stopped = false;

    /** The door of a root conversation, opened the first time it comes alive here. */
    const doorOf = (root: string): Door => {
      let door = process_.doors.get(root);
      if (!door) {
        // A process that inherited a lead naming another conversation works for it.
        const theirs = lead && !(lead.harness === HARNESS && lead.external_id === root) ? lead : undefined;
        door = new Door(root, ctx, mode, theirs, cwd);
        process_.doors.set(root, door);
        void ctx.session
          .get({ sessionID: root })
          .then((info) => {
            door!.selected ??= modelOf(info.model);
            door!.noteState();
          })
          .catch(() => {});
        door.dial();
      }
      return door;
    };

    // Each plugin instance of this process sees every event; the instance that opened a
    // door answers for it, so no event is read twice.
    const mine = new Set<string>();
    const ownDoor = (root: string): Door | undefined => {
      if (!process_.doors.has(root)) mine.add(root);
      return mine.has(root) ? doorOf(root) : undefined;
    };

    // Events go through one line, in the order they came.
    let line = Promise.resolve();
    const listen = async () => {
      while (!stopped) {
        try {
          for await (const event of ctx.event.subscribe()) {
            if (stopped) return;
            const { type, data, created } = fields(event) as { type?: unknown; data?: unknown; created?: unknown };
            if (typeof type !== "string") continue;
            const props = fields(data);
            const session = sessionOf(type, props);
            if (!session) continue;
            if (type === "session.created") {
              const parentID = text(props.parentID);
              if (parentID) process_.parents.set(session, rootOf(ctx, parentID));
            }
            const at = count(created) ?? Date.now();
            line = line
              .then(async () => {
                const root = await rootOf(ctx, session);
                if (!root) return;
                if (type === "session.deleted" && (root !== session || !process_.doors.has(root))) return;
                // A conversation comes alive by working: a sub-agent's first event opens its lead's door.
                if (root === session && !process_.doors.has(root) && type === "session.created") return;
                ownDoor(root)?.event(type, props, session, at);
              })
              .catch(() => {});
          }
        } catch {
          // The stream ended; listen again.
        }
        await new Promise((resolve) => setTimeout(resolve, RESUBSCRIBE_MS));
      }
    };
    void listen();

    await ctx.session.hook("prompt", async (input) => {
      try {
        const root = await rootOf(ctx, input.sessionID);
        if (!root || root !== input.sessionID) return;
        const door = ownDoor(root);
        if (door && compact) {
          const file = compact;
          compact = undefined;
          await compactAsked(file, door);
        }
      } catch {
        // The prompt goes on as it was.
      }
    });

    await ctx.session.hook("experimental.idle", async (input) => {
      try {
        const root = await rootOf(ctx, input.sessionID);
        if (root !== input.sessionID) return;
        // The events before the idle are read first.
        await line;
        await process_.doors.get(root)?.settle(input.background.length);
      } catch {
        // The turn ends as it would.
      }
    });

    return () => {
      stopped = true;
      for (const root of mine) {
        process_.doors.get(root)?.shut();
        process_.doors.delete(root);
      }
    };
  },
};

export default plugin;
