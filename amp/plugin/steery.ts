/**
 * Steery's plugin for Amp: the door of PROTOCOL.md §4.4, version 1. Amp runs each plugin in a
 * process of its own, a child of the `amp` process (the TUI, or an `amp -x` run). For each
 * root thread that starts there, this plugin dials the runtime's loopback door, says what
 * the thread is doing (`state`), and answers the runtime's asks through Amp's plugin API.
 *
 * Amp keeps its threads on ampcode.com, which the driver reads through Amp's CLI, so this
 * door carries no entries. Amp sends a plugin no text while a message forms, so it carries
 * no `live` frames. A thread with a parent thread is Amp's own sub-agent: it opens no door.
 */
import type { AgentEndEvent, PluginAPI, ThreadMessage } from "@ampcode/plugin";
import { spawnSync } from "node:child_process";
import { readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

/** The runtime's door, filled in by the driver's `setup/plan`. */
const DOOR = "__STEERY_DOOR__";
const HARNESS = "amp";
const DOOR_VERSION = 1;

const REDIAL_MIN_MS = 250;
const REDIAL_MAX_MS = 2000;
const OTHER_DOOR_MS = 5000;
const HELLO_UNREADABLE = 4400;
const OTHER_DOOR = 4409;
const REPLACED = 4410;

const STATE_EVERY_MS = 100;
const ANSWERS_KEPT = 256;
const ENDED_KEPT = 64;
const SETTLED_KEPT = 16;
/** How long handed words may take to show as held before the say is `no-answer`. */
const HELD_WITHIN_MS = 8000;
const STOPPED_WITHIN_MS = 2500;
/** How long a cancelled turn's steered words may take to start the next turn. */
const LET_GO_AFTER_MS = 1000;
const SETTLE_WAIT_MS = 1000;
const ERROR_MAX = 600;

/** Amp names an MCP tool `mcp__<server>__<tool>`; Steery's server is this one. */
const STEERY_TOOL = "mcp__steery__";
/** Amp's shell tools, whose `input.command` is the command line. */
const SHELLS: Record<string, true> = { Bash: true, shell_command: true };

type Json = Record<string, unknown>;
type Fields = { readonly [key: string]: unknown };
type Mode = "tui" | "rpc" | "json" | "print";
type Answer = { ok: unknown } | { refused: string; error?: string };
type Lead = { harness: string; external_id: string };

function fields(value: unknown): Fields {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Fields) : {};
}

const text = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined);

/** One line of `ps` for a pid: its parent and its command line. */
function ps(pid: number): { parent: number; command: string } | undefined {
  try {
    const out = spawnSync("ps", ["-o", "ppid=,command=", "-p", String(pid)], { encoding: "utf8" }).stdout.trim();
    const [, parent, command] = /^\s*(\d+)\s+(.*)$/.exec(out) ?? [];
    return parent ? { parent: Number(parent), command: command ?? "" } : undefined;
  } catch {
    return undefined;
  }
}

/** The `amp` process this plugin serves: Amp runs each plugin as a child of it. */
const harness = { pid: process.ppid, ...(ps(process.ppid) ?? { parent: 0, command: "" }) };

/**
 * `STEERY_STARTED=<pid>:<what>`, trusted where the `amp` process's own parent is that pid:
 * the plugin inherits the variable from `amp`, one process further down.
 */
function startedBy(): string | undefined {
  const value = process.env.STEERY_STARTED;
  const at = value?.indexOf(":") ?? -1;
  if (!value || at < 1) return undefined;
  const pid = Number(value.slice(0, at));
  return pid > 1 && pid === harness.parent ? value.slice(at + 1) : undefined;
}

/**
 * Who is at the `amp` process, from its command line: `-x` (`--execute`) is a one-shot run,
 * `json` where it streams JSON; anything else is Amp's TUI.
 */
function modeOf(started: string | undefined): Mode {
  if (started?.startsWith("turn:") || started?.startsWith("turn@")) return "json";
  const args = harness.command.split(/\s+/);
  if (!args.includes("-x") && !args.includes("--execute")) return "tui";
  return args.includes("--stream-json") ? "json" : "print";
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

/** A user message's words, its text blocks joined as Amp shows them. */
function wordsOf(message: ThreadMessage): string | undefined {
  if (message.role !== "user") return undefined;
  const blocks = message.content.flatMap((block) => (block.type === "text" ? [block.text] : []));
  return blocks.length ? blocks.join("\n") : undefined;
}

/** Words Steery says into the thread, marked as Steery's. */
const reminder = (words: string) => `<system-reminder reason="steery">\n${words}\n</system-reminder>`;

/** Words of a `say`, from the ask until Amp takes them or lets them go. */
type Say = {
  id: string;
  also: string[];
  words: string;
  mode: "steer" | "queue";
  queuedAt?: number;
  takenAt?: number;
  /** Handed while a turn ran: Amp queues them as a steering message. */
  steered: boolean;
  handed: boolean;
  letGo: boolean;
  failed?: string;
  handedAgain: boolean;
  wake: Array<() => void>;
};

const wakeAll = (say: Say) => {
  for (const wake of say.wake.splice(0)) wake();
};

/** One root thread of this `amp` process, and its door. */
class Door {
  socket: WebSocket | undefined;
  welcomed = false;
  wanted = true;
  unheard = false;
  backoff = REDIAL_MIN_MS;
  redial: NodeJS.Timeout | undefined;

  turn: { began_at: number; says: string[]; steerys: boolean } | undefined;
  turns = 0;
  held: Say[] = [];
  says = new Map<string, Say>();
  throws = new Map<string, Say>();
  answers = new Map<string, Promise<Answer>>();
  ended: Array<{ id: string; at: number }> = [];
  settled: Json[] = [];
  continuations: string[] = [];
  turnEnded: Array<() => void> = [];
  asked = false;
  parked = false;
  waiting = false;
  continued = false;
  settling = new Map<string, (ok: unknown) => void>();
  sentState = "";
  stateTimer: NodeJS.Timeout | undefined;
  stateAt = 0;

  constructor(
    readonly id: `T-${string}`,
    readonly amp: PluginAPI,
    readonly mode: Mode,
    readonly lead: Lead | undefined,
    readonly cwd: string | undefined,
  ) {}

  oneShot(): boolean {
    return this.mode === "json" || this.mode === "print";
  }

  pending(): boolean {
    return this.held.length > 0 || [...this.says.values()].some((say) => say.handed && !say.takenAt && !say.letGo);
  }

  idle(): boolean {
    return !this.turn && !this.pending();
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
    socket.onopen = () =>
      this.send({
        t: "hello",
        door: DOOR_VERSION,
        harness: HARNESS,
        pid: harness.pid,
        session_id: this.id,
        mode: this.mode,
        ...(this.lead ? { lead: this.lead } : {}),
        state: this.state(),
        ...(this.cwd ? { cwd: this.cwd } : {}),
      });
    socket.onmessage = (message) => this.arrived(message.data);
    socket.onclose = (closed) => {
      if (this.socket === socket) this.dropped(closed.code);
    };
    socket.onerror = () => {};
  }

  dropped(code: number | undefined): void {
    const was = this.welcomed;
    this.socket = undefined;
    this.welcomed = false;
    for (const wake of this.settling.values()) wake(undefined);
    this.settling.clear();
    if (code === HELLO_UNREADABLE) this.unheard = true;
    if (!this.wanted || this.unheard || code === REPLACED) return;
    if (was) this.backoff = REDIAL_MIN_MS;
    const wait = code === OTHER_DOOR ? OTHER_DOOR_MS : this.backoff;
    this.backoff = Math.min(this.backoff * 2, REDIAL_MAX_MS);
    this.redial = setTimeout(() => this.dial(), wait);
    this.redial.unref();
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
      this.backoff = REDIAL_MIN_MS;
      if (frame.entries) {
        this.send({ t: "lost", why: "Amp keeps its threads on ampcode.com, which the driver reads; this door carries no entries" });
      }
      this.sentState = "";
      this.noteState();
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

  state(): Json {
    return {
      ...(this.turn ? { turn: { began_at: this.turn.began_at, says: this.turn.says } } : {}),
      idle: this.idle(),
      ...(this.pending() ? { pending: true } : {}),
      // A one-shot run has no editor anywhere: nobody types into it.
      ...(this.oneShot() ? { typing: false } : {}),
      compacting: false,
      retrying: false,
      // Amp tells a plugin when a call ends, never while it runs.
      calls: [],
      ended: this.ended,
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
      const sent = JSON.stringify(state);
      if (sent === this.sentState) return;
      this.sentState = sent;
      this.stateAt = Date.now();
      this.send({ t: "state", state });
    }, wait);
  }

  // ---- what Amp says --------------------------------------------------------------

  /** `agent.start`: Amp took a prompt and its turn begins. */
  began(words: string): void {
    this.turn = { began_at: Date.now(), says: [], steerys: false };
    this.turns++;
    this.turn.steerys = this.took(words);
    this.noteState();
  }

  /**
   * Words Amp took into a turn: a say's, a continuation's, or the person's own. Amp names
   * no say, so the oldest handed say with exactly these words is the one taken. Whether
   * they were Steery's.
   */
  took(words: string): boolean {
    const say = [...this.says.values()].find((say) => say.handed && !say.takenAt && !say.letGo && say.words === words);
    const continuation = this.continuations.indexOf(words);
    if (say) {
      say.takenAt = Date.now();
      if (this.turn) for (const id of [say.id, ...say.also]) if (!this.turn.says.includes(id)) this.turn.says.push(id);
      wakeAll(say);
    } else if (continuation >= 0) {
      this.continuations.splice(continuation, 1);
    } else {
      // The person's own words: a park and a continuation hold until they speak.
      this.parked = false;
      this.continued = false;
    }
    this.asked = false;
    this.waiting = false;
    return !!say || continuation >= 0;
  }

  /** `agent.end`: the turn ended. Steered words its messages hold were taken in it. */
  endedTurn(event: AgentEndEvent): void {
    for (const message of event.messages.slice(1)) {
      const words = wordsOf(message);
      if (words !== undefined) this.took(words);
    }
    const turn = this.turn;
    this.turn = undefined;
    if (turn?.steerys) {
      const failed = event.status === "error";
      this.settled.push({ began_at: turn.began_at, at: Date.now(), says: turn.says, failed });
      while (this.settled.length > SETTLED_KEPT) this.settled.shift();
    }
    if (event.status === "cancelled") {
      // Steered words that no turn takes soon after a cancel went with it.
      const steered = [...this.says.values()].filter((say) => say.steered && !say.takenAt && !say.letGo);
      setTimeout(() => {
        for (const say of steered) {
          if (say.takenAt || this.turn) continue;
          say.letGo = true;
          wakeAll(say);
        }
        this.noteState();
      }, LET_GO_AFTER_MS);
    }
    for (const wake of this.turnEnded.splice(0)) wake();
    // Held words go in once the handler has let the turn end.
    setTimeout(() => void this.handHeld(), 0);
    this.noteState();
  }

  /** `tool.result`: a call ended; a Steery call says whether the turn asked, waits, or parked. */
  callEnded(id: string, tool: string, input: Fields, output: unknown): void {
    this.ended.push({ id, at: Date.now() });
    while (this.ended.length > ENDED_KEPT) this.ended.shift();
    const said = typeof output === "string" ? output : JSON.stringify(output ?? "");
    const command = SHELLS[tool] ? ran(input.command) : undefined;
    const inner = tool.startsWith(STEERY_TOOL) ? tool.slice(STEERY_TOOL.length) : undefined;
    if (command || inner !== undefined) {
      const op = command?.op ?? inner!.replace("_", ".");
      if (op === "requests.create" || op === "requests.get") this.asked = true;
      this.waiting = waits(command ?? { op, words: JSON.stringify(input) }, said);
      if (inner === "sessions_describe" && fields(input.park) === input.park && !/"error"|refus/i.test(said.slice(0, 200))) {
        this.parked = true;
      }
    }
    this.noteState();
  }

  // ---- settle ---------------------------------------------------------------------

  /**
   * A turn of the person's TUI finished on its own: ask the runtime whether Steery's words
   * keep it going, wait at most 1 s, and continue with them (`agent.end`'s `continue`). A
   * one-shot run asks nothing: it exits as its turn ends.
   */
  async settle(event: AgentEndEvent): Promise<string | undefined> {
    if (!this.welcomed || this.lead || this.oneShot()) return undefined;
    if (event.status !== "done" || this.held.length) return undefined;
    const id = `settle-${event.id}`;
    const { promise, resolve } = Promise.withResolvers<unknown>();
    const timer = setTimeout(() => resolve(undefined), SETTLE_WAIT_MS);
    this.settling.set(id, resolve);
    this.send({ t: "settle", id, asked: this.asked, parked: this.parked, waiting: this.waiting, continued: this.continued });
    const ok = await promise;
    clearTimeout(timer);
    this.settling.delete(id);
    const say = text(fields(ok).say);
    if (!say?.trim()) return undefined;
    this.continued = true;
    const words = reminder(say);
    this.continuations.push(words);
    return words;
  }

  // ---- the runtime's asks ---------------------------------------------------------

  async ask(frame: Fields): Promise<Answer> {
    switch (frame.op) {
      case "say":
        return this.say(frame);
      case "pick":
        return { refused: "selection-unsupported", error: "an Amp thread keeps the mode it began in" };
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
    if (say.failed) return { refused: "refused", error: say.failed };
    if (say.takenAt) return { ok: { queued_at: say.queuedAt ?? say.takenAt, taken_at: say.takenAt } };
    if (say.letGo) return { refused: "not-taken", error: "Amp let the words go with its cancelled turn" };
    return undefined;
  }

  /** Wait for the say to be taken, or (for steered words) held, at most 8 s to be held. */
  async settledSay(say: Say): Promise<Answer> {
    const deadline = Date.now() + HELD_WITHIN_MS;
    for (;;) {
      const answer = this.answerOf(say);
      if (answer) return answer;
      if (say.steered && say.queuedAt) return { ok: { queued_at: say.queuedAt } };
      const left = say.handed ? 60_000 : deadline - Date.now();
      if (left <= 0) return { refused: "no-answer", error: "Amp did not show the words held within 8 s" };
      const { promise, resolve } = Promise.withResolvers<void>();
      const timer = setTimeout(resolve, left);
      say.wake.push(() => {
        clearTimeout(timer);
        resolve();
      });
      await promise;
    }
  }

  async say(frame: Fields): Promise<Answer> {
    if (this.lead) return { refused: "refused", error: "a sub-agent's thread takes words only at its lead" };
    const id = text(frame.id) ?? "";
    const words = text(frame.words);
    const known = this.says.get(id);
    if (known) return this.answerOf(known) ?? this.settledSay(known);
    if (!words) return { refused: "refused", error: "no words, and this copy does not remember that say" };
    const mode = frame.mode === "queue" ? "queue" : frame.mode === "steer" ? "steer" : undefined;
    if (!mode) return { refused: "refused", error: "mode must be steer or queue" };
    if (frame.idle === true && !this.idle()) return { refused: "harness-busy", error: "a turn runs or words wait" };
    if (this.oneShot() && this.turns === 0) return { refused: "refused", error: "a one-shot run takes no words before its own turn" };
    if (this.oneShot() && !this.turn) return { refused: "ending", error: "this one-shot run's turn has ended" };
    const thrown = text(frame.throw);
    const first = thrown ? this.throws.get(thrown) : undefined;
    if (first) {
      if (!first.also.includes(id)) first.also.push(id);
      this.says.set(id, first);
      if (this.turn && first.takenAt && this.turn.says.includes(first.id)) this.turn.says.push(id);
      return this.answerOf(first) ?? this.settledSay(first);
    }
    const say: Say = { id, also: [], words, mode, steered: false, handed: false, letGo: false, handedAgain: false, wake: [] };
    if (thrown) this.throws.set(thrown, say);
    this.says.set(id, say);
    if (mode === "queue" && this.turn) {
      this.held.push(say);
      this.noteState();
      return this.settledSay(say);
    }
    await this.hand(say);
    this.noteState();
    return this.settledSay(say);
  }

  /** Hand the words to Amp: as a steering message into the running turn, or to start one. */
  async hand(say: Say): Promise<void> {
    say.steered = !!this.turn;
    say.letGo = false;
    try {
      await this.amp.threads.get(this.id).appendUserMessage({ type: "user-message", content: say.words }, { steer: say.steered });
      say.handed = true;
      say.queuedAt = Date.now();
    } catch (error) {
      say.failed = String(error instanceof Error ? error.message : error).slice(0, ERROR_MAX);
    }
    wakeAll(say);
  }

  /** Words held for the end of a turn go in once no turn runs. */
  async handHeld(): Promise<void> {
    const say = this.turn ? undefined : this.held.shift();
    if (!say) return;
    await this.hand(say);
    this.noteState();
  }

  /** Cancel the running turn; whether it ended within 2.5 s. */
  async cancel(): Promise<boolean> {
    const ended = Promise.withResolvers<void>();
    this.turnEnded.push(ended.resolve);
    await this.amp.threads.get(this.id).cancel();
    const waited = Promise.withResolvers<void>();
    setTimeout(waited.resolve, STOPPED_WITHIN_MS);
    await Promise.race([ended.promise, waited.promise]);
    return !this.turn;
  }

  async stop(sayId: unknown): Promise<Answer> {
    if (this.lead) return { refused: "refused", error: "a sub-agent's thread stops only at its lead" };
    if (sayId !== undefined && (typeof sayId !== "string" || !sayId)) {
      return { refused: "refused", error: "a stop names the say whose turn it stops, or no say" };
    }
    // The person's pause from Steery parks the thread whether or not a turn runs.
    if (sayId === undefined) this.parked = true;
    if (!this.turn) return { ok: { stopped: false, running: [] } };
    if (typeof sayId === "string" && !this.turn.says.includes(sayId)) {
      return { refused: "other-turn", error: "the turn that runs is not one its words began" };
    }
    if (!(await this.cancel())) return { refused: "no-answer", error: "a turn still runs after 2.5 s" };
    return { ok: { stopped: true, running: [] } };
  }

  async force(sayId: unknown): Promise<Answer> {
    if (this.lead) return { refused: "refused", error: "a sub-agent's thread takes this only at its lead" };
    const say = typeof sayId === "string" ? this.says.get(sayId) : undefined;
    if (!say || say.letGo || say.failed) return { refused: "refused", error: "no words of that say wait here" };
    if (say.takenAt || !this.turn) return { ok: { stopped: false, running: [] } };
    const held = this.held.indexOf(say);
    if (held >= 0) this.held.splice(held, 1);
    if (!(await this.cancel())) return { refused: "no-answer", error: "a turn still runs after 2.5 s" };
    // Amp can drop its steering queue with the cancel; words no turn took go in again, once.
    const waited = Promise.withResolvers<void>();
    setTimeout(waited.resolve, LET_GO_AFTER_MS);
    await waited.promise;
    if (!say.takenAt && !say.handedAgain && !this.turn) {
      say.handedAgain = true;
      await this.hand(say);
    }
    return { ok: { stopped: true, running: [] } };
  }

  /**
   * Amp cannot put a given summary in place at a given cut without running its own model,
   * so it declines: the context starts at the thread's first message.
   */
  async compact(): Promise<Answer> {
    if (this.lead) return { refused: "refused", error: "a sub-agent's thread is compacted at its lead" };
    if (this.turn) return { refused: "harness-busy", error: "a turn runs" };
    const [first] = await this.amp.threads.get(this.id).messages({ from: "start", limit: 1 });
    const from = first?.createdAt ? Date.parse(first.createdAt) : Date.now();
    return { ok: { from: Number.isFinite(from) ? from : Date.now(), forgot: 0 } };
  }
}

/**
 * A `compact` ask a turn the runtime started carries in a file (`STEERY_COMPACT`), answered
 * as the door answers one, before the turn's prompt runs. The answer frame goes to
 * `compacted.json` beside the ask, readable by its owner only; an ask that cannot be read is
 * answered nothing. The plugin's own process drops the variable; the `amp` process keeps it.
 */
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

export const description = "Steery: reaches each Amp thread through the Steery runtime's door";

export default function (amp: PluginAPI) {
  const started = startedBy();
  if (started === "nothing") return;
  const mode = modeOf(started);
  const lead = (() => {
    const value = process.env.STEERY_LEAD;
    const at = value?.indexOf(":") ?? -1;
    return value && at > 0 ? { harness: value.slice(0, at), external_id: value.slice(at + 1) } : undefined;
  })();
  let compactFile = process.env.STEERY_COMPACT || undefined;
  delete process.env.STEERY_COMPACT;
  const root = amp.system.workspaceRoot;
  const cwd = root ? amp.helpers.filePathFromURI(root) : undefined;
  const doors = new Map<string, Door | null>();

  /** The door of a root thread, opened the first time it starts here; none for a sub-agent's. */
  const doorOf = async (thread: { id: `T-${string}`; parentThreadID(): Promise<string | null> }): Promise<Door | undefined> => {
    const known = doors.get(thread.id);
    if (known !== undefined) return known ?? undefined;
    const parent = await thread.parentThreadID().catch(() => null);
    if (doors.has(thread.id)) return doors.get(thread.id) ?? undefined;
    if (parent) {
      doors.set(thread.id, null);
      return undefined;
    }
    // A process that inherited a lead naming another conversation works for it.
    const theirs = lead && !(lead.harness === HARNESS && lead.external_id === thread.id) ? lead : undefined;
    const door = new Door(thread.id, amp, mode, theirs, cwd);
    doors.set(thread.id, door);
    door.dial();
    return door;
  };

  amp.on("session.start", async (_event, ctx) => {
    await doorOf(ctx.thread);
  });

  amp.on("agent.start", async (event, ctx) => {
    const door = await doorOf(ctx.thread);
    if (!door) return {};
    if (compactFile) {
      const file = compactFile;
      compactFile = undefined;
      await compactAsked(file, door);
    }
    door.began(event.message);
    return {};
  });

  amp.on("tool.result", (event, ctx) => {
    void doorOf(ctx.thread).then((door) => door?.callEnded(event.toolUseID, event.tool, fields(event.input), event.output));
  });

  amp.on("agent.end", async (event, ctx) => {
    const door = await doorOf(ctx.thread);
    if (!door) return;
    door.endedTurn(event);
    const words = await door.settle(event).catch(() => undefined);
    return words ? { action: "continue", userMessage: words } : undefined;
  });
}
