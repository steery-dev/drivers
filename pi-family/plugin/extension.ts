// Steery's extension, written by the runtime once per version, and run by the loader
// `steery setup` writes into each harness's `extensions/` (`loader.ts`).
//
// One module for pi and for omp, which is pi's fork and keeps its extension API: one copy
// per harness, naming the harness below, and the loader moves every session of a process
// onto a newer copy as soon as the runtime names one: a copy lets go of all it holds when
// it is stopped, and the next takes the session up where it stood ([`Carried`]).
//
// Each conversation's copy opens one door: a WebSocket to this machine's runtime on its
// loopback listener, speaking the plugin door, version 2 (PROTOCOL.md §4.4). Over it the
// copy says what the conversation is doing as it changes (`state`, a LiveState) and the
// harness's moments as they happen (`moment`, the hook document the driver's `hook/read`
// reads); at each turn the person starts the runtime answers the one paragraph Steery has
// for its agent (`say`), and at a turn's end (`settle`) the words that keep it going. In a
// conversation Steery started, the runtime asks the copy over the same door to hand words
// to it (`say`), switch its model (`pick`), stop a turn (`stop`), recap it (`recap`) and
// compact it (`compact`). The copy draws nothing in the harness and takes no answer there:
// a person answers Steery on Steery's own surfaces. pi's copy also carries Steery's MCP
// entry, registering it for each conversation with headers naming it; omp lets no
// extension register one, so its copy carries none and omp reaches Steery through the
// entry `steery setup` writes into its own `mcp.json`. Each place the two harnesses' APIs
// differ is a branch on `HARNESS`, here and nowhere else. Nothing here decides anything or
// stands in a call's way: every handler
// nowhere else. Nothing here decides anything or stands in a call's way: every handler
// observes, the only waits are short and bounded — a moment's answer, a settle's, and a
// conversation's last moments as it closes — a failure is swallowed, and a Steery that
// is gone leaves the harness exactly as it would be without this file.

import { closeSync, constants, existsSync, openSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";

/** `O_CLOEXEC` from `<fcntl.h>`, which `node:fs` does not export. */
const O_CLOEXEC = process.platform === "darwin" ? 0x1000000 : 0o2000000;

/** `pi` or `omp`: the harness this copy was written for. */
const HARNESS: string = "__STEERY_HARNESS__";

/** The plugin door version this copy speaks (PROTOCOL.md §4.4). */
const DOOR_VERSION = 2;

/** The runtime's door. */
const DOOR: string = "__STEERY_DOOR__";

/**
 * Steery's MCP entry, as `steery setup` would have written it into `mcp.json`, where this
 * copy registers it itself (pi); none where the harness reads its own file (omp).
 */
const MCP: Record<string, unknown> = entry("__STEERY_MCP__");

/** Whether this copy registers Steery's MCP entry for each conversation. */
const REGISTERS = Object.keys(MCP).length > 0;

/** How many of Steery's says the door keeps until it sees their words go in. */
const SAYS_KEPT = 32;

/**
 * How long words put in through the door may take to be taken by the harness before the
 * door says they were not: under the ten seconds the runtime waits for its answer.
 */
const HEARD_WITHIN_MS = 8000;

/**
 * How long a stop waits for the conversation to go idle before it says a turn still runs:
 * under the 2.8 s the runtime waits for its answer, inside the one stop's 3 s graceful
 * window, after which the runtime ends the process anyway (`attach/native.rs` `GRACEFUL`).
 */
const STOPPED_WITHIN_MS = 2500;

/**
 * How long a `compact` ask may take the harness ([`compact`]): under the thirty seconds the
 * runtime waits for its answer.
 */
const COMPACTED_WITHIN_MS = 25_000;

/** What Steery's own compaction carries as the harness's `customInstructions`, before the ask's id. */
const COMPACT_MARK = "steery-compact:";

/** What pi counts an image as, in characters, for its four-to-a-token estimate. */
const IMAGE_CHARS = 4800;

/** Characters to a token where a branch's model counted none of it ([`tokenRate`]): pi's own estimate. */
const CHARS_PER_TOKEN = 4;

/**
 * The least share of what the context holds after a compaction that the compaction must
 * forget while the provider still caches the context ([`worth`]). Every call after it then
 * reads what it forgot no more, at a cache read each, but the first writes all that stays to
 * the cache again, twenty times a read for Anthropic's one-hour entries. Over Steery's 145
 * compactions on one Mac on 2026-10-15, the 27 a half would have stopped lost $7.37 in 18
 * and saved $2.57 in 9.
 */
const WARM_SHARE = 0.5;

/** How long a provider caches a context after its last call: five minutes, or an hour where its writes say so ([`worth`]). */
const CACHED_MS = 5 * 60_000;
const CACHED_HOUR_MS = 60 * 60_000;

/**
 * The turn and its answer a compacted context opens with, before its summary ([`opened`]).
 * A proxy that cloaks the harness as Claude Code on a Claude subscription (CLIProxyAPI) sends
 * the system prompt as a turn after the first person's turn, so a compaction changed what
 * comes before it and the call after wrote it to the cache again; after these it stays
 * cached, its breakpoint kept ([`unanchored`]). Through omp on 2026-10-16, the call after a
 * new summary read 22,427 tokens from the cache and wrote 1,369 with both; with either
 * missing it read 8,339, the tools alone, and wrote about 15,400.
 */
const OPENING = "This conversation was compacted: the next message summarises its earlier work.";
const OPENED = "Understood.";

/** The fields of a message and its parts whose characters a model reads. */
const READ_FIELDS: Record<string, true> = {
  content: true,
  text: true,
  thinking: true,
  name: true,
  command: true,
  output: true,
  summary: true,
};

/**
 * How long a pick waits for the harness to read its model files again ([`reread`]): with
 * the rest of the pick, under the ten seconds the runtime waits for its answer.
 */
const REREAD_MS = 5000;

/**
 * How long the conversation's own recap may take ([`recap`]): under the forty seconds the
 * runtime waits for it.
 */
const RECAP_MS = 35000;

/** What the conversation's own model is asked for a recap, after its own history. */
const RECAP_ASKED =
  "Steery is carrying this conversation to another machine, where a new agent goes on from it. " +
  "Do not use any tool and do not carry on with the work. Write a recap for that agent, in a few " +
  "paragraphs: what the person asked for, decisions and why, work completed, the exact files and " +
  "commands that matter, unresolved errors, and what remains. Say what was verified and what was " +
  "not. Stay under 1500 words. No credentials, tokens or keys. Answer with the recap alone.";

/** How many answered asks the door remembers, so an ask sent again is answered once. */
const REMEMBERED = 256;

/** How long a turn the person starts waits for Steery's paragraph before going on without it. */
const PROMPT_MS = 1500;

/** How long a call waits for what Steery has to say beside it before going on without it. */
const CALL_MS = 500;

/**
 * How long a call to one of Steery's own tools waits for the runtime to have written down
 * which conversation makes it, where the MCP connection names none. The runtime answers a
 * moment once it has, and a call sent before that is made as nobody's: in the machine's
 * Personal workspace, where a read of this conversation's data is 404 or empty. Under a
 * busy runtime the answer was measured 1.3–6.5 s late on 2026-10-15.
 */
const STAMP_MS = 15_000;

/** How long a turn's end waits for Steery's words to keep it going before ending without them. */
const SETTLE_MS = 1000;

/** How long a conversation's end waits for its moments to reach the runtime. */
const FLUSH_MS = 500;

/**
 * The custom entry a pi run that settled with its context full ends on: pi's own overflow
 * recovery failed. The machine's witness reads it by this name (`PI_CONTEXT_FULL`).
 */
const CONTEXT_FULL = "steery-context-full";

/**
 * The custom entry written just before a compaction that is not Steery's, saying who made
 * it: `{by: "harness", reason}` where the harness compacted on its own, `{by: "person"}`
 * where it was asked. The driver's reader takes it by this name (`COMPACTION`) for the
 * compaction entry after it.
 */
const COMPACTION = "steery-compaction";

/** The first wait before dialing the door again, doubled at each failure up to the next. */
const REDIAL_MIN_MS = 250;
const REDIAL_MAX_MS = 2000;

/** The wait after the runtime refused this copy's door version: by then a newer copy may be loaded. */
const OTHER_DOOR_MS = 5000;

/** The close code of a hello the runtime could not read, or that names no plugin driver: never dialed again. */
const UNHEARD = 4400;

/** The close code of a runtime that does not speak this copy's door version. */
const OTHER_DOOR = 4409;

/** The close code of a door a newer one of the same process and conversation replaced. */
const REPLACED = 4410;

/** The close code this copy closes its door with: its conversation ended here or moved. */
const LET_GO = 1000;

/** At most one `state` per this many milliseconds; changes in between go in the next. */
const STATE_EVERY_MS = 100;

/**
 * How many ended calls the door's `state` names, newest kept: pi writes the results of a
 * batch of calls run side by side only once the whole batch is over, so the runtime reads
 * when each really ended here.
 */
const ENDED_KEPT = 64;

/**
 * How many of Steery's turns that ended the door's `state` names, newest kept: how each
 * ended is the run its say's aim reports, and a runtime that was away reads it here.
 */
const SETTLED_KEPT = 16;

/**
 * How often what nothing announces — somebody typing, words queued — is looked at
 * again while the door is open. Sent only when it changed.
 */
const LOOK_EVERY_MS = 1000;

/** How many moments the process keeps while no door is open, oldest dropped first. */
const MOMENTS_KEPT = 256;

/** The MCP entry this copy carries, or none where it carries none. */
function entry(json: string): Record<string, unknown> {
  try {
    const parsed: unknown = JSON.parse(json);
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}

type Usage = { tokens?: number | null; contextWindow?: number } | undefined;

/** A model as both harnesses hold one; Steery names it `<provider>/<id>`. */
type Model = { provider: string; id: string; /** The wire its provider speaks. */ api?: string };

/** One model request's answer, as both harnesses return it: its text parts and how it stopped. */
type Answered = { content?: { type?: string; text?: string }[]; stopReason?: string; errorMessage?: string };

/** The session's own input line, where it has one: the same in pi's terminal and its RPC clients. */
type Ui = {
  getEditorText?(): string;
};

type Context = {
  cwd: string;
  /** `tui`, `rpc`, `json` or `print` in both harnesses; the last two are one-shot runs. */
  mode?: string;
  /** omp only: which agent this session runs. A `sub` one is another's sub-agent. */
  agent?: { kind?: string; id?: string };
  /** Whether a person can type into this session: a terminal or a client's dialogs. */
  hasUI?: boolean;
  /** The session's own input line, where it has one. */
  ui?: Ui;
  sessionManager: {
    getSessionId(): string;
    getSessionFile?(): string | undefined;
    /** omp: the session's entries, oldest first. */
    getEntries?(): unknown[];
    /** Both: the entries from the first to where the conversation stands now, oldest first. */
    getBranch?(): unknown[];
    /**
     * Both: what the model is handed on the conversation's next turn — its messages,
     * compacted where it was — as the harness builds it itself.
     */
    buildSessionContext?(): { messages?: unknown[] };
    /** pi 1.0.0: the same, with where each message came from. */
    buildSessionProjection?(): { messages?: unknown[] };
  };
  /** Both: the system prompt its turns run under now. */
  getSystemPrompt?(): string;
  getContextUsage?(): Usage;
  isIdle?(): boolean;
  hasPendingMessages?(): boolean;
  abort?(): void;
  /**
   * Compact the conversation now; a string is omp's `customInstructions`. omp 18.10 settles
   * once it compacted and rejects where it would not; pi 1.0 returns at once and says so
   * through `onComplete` or `onError`.
   */
  compact?(
    options?:
      | string
      | { customInstructions?: string; onComplete?: (result: unknown) => void; onError?: (error: Error) => void },
  ): Promise<void> | void;
  /**
   * omp: what it runs past a turn — background jobs, async sub-agents — which an abort
   * leaves running, and which wake the conversation when they end; and how many of their
   * results still wait to be handed to it (`delivery`).
   */
  getAsyncJobSnapshot?():
    | {
        running?: { id?: string; type?: string; label?: string }[];
        delivery?: { queued?: number; delivering?: boolean };
      }
    | undefined;
  /** The session's model now: a getter in both harnesses. */
  model?: Model;
  /** omp: its own lookup, which reads a selector the way `--model` does. */
  models?: { current?(): Model | undefined; resolve?(spec: string): Model | undefined };
  /**
   * Its catalog, by provider and id, and what it knows of each model's credentials: in
   * both harnesses the same calls, and the two questions each asks itself before it starts
   * a turn.
   */
  modelRegistry?: {
    find?(provider: string, id: string): Model | undefined;
    hasConfiguredAuth?(model: Model): boolean;
    getApiKeyAndHeaders?(model: Model): Promise<{ ok: boolean; error?: string }>;
    /**
     * Read the model files again and rebuild the catalog from them, the session, its
     * model and its turn left as they are: pi 1.0.0 re-reads `models.json` and composes
     * its providers again, an extension's among them, and takes a `signal`; omp 18.4.12
     * re-reads `models.yml`, reads its first argument as a strategy, and takes no signal.
     * omp watches its `config.yml` itself.
     */
    refresh?(options?: { signal?: AbortSignal }): Promise<unknown>;
    /**
     * pi 1.0.0: one request to a model through its provider, authenticated as a turn
     * is, nothing written into the conversation.
     */
    complete?(model: Model, context: unknown, options?: { signal?: AbortSignal }): Promise<Answered>;
    /** omp 18.4.12: the key a request to `model` is authenticated with, as its turns use. */
    resolver?(model: Model, sessionId?: string): unknown;
  };
};

/**
 * Why the harness would refuse to start a turn from words given now, or nothing where it
 * would start one or cannot be asked. Each asks this itself before it starts one — a
 * model, and credentials its provider can find — and drops the words where either is
 * missing: pi after its `input` event, out of sight of whoever handed them over (pi 1.0.0
 * `AgentSession.prompt`), omp by throwing from the call that handed them, before it takes
 * them (omp 18.4.12 `AgentSession.prompt`).
 */
async function unstartable(ctx: Context): Promise<string | undefined> {
  const model = ctx.model;
  if (!model) return `${HARNESS} has no model selected`;
  const registry = ctx.modelRegistry;
  if (!registry?.hasConfiguredAuth || !registry.getApiKeyAndHeaders) return undefined;
  try {
    if (registry.hasConfiguredAuth(model)) return undefined;
    const auth = await registry.getApiKeyAndHeaders(model);
    return auth.ok ? undefined : `${HARNESS} has no credentials for ${model.provider}/${model.id}: ${auth.error ?? "none found"}`;
  } catch (error) {
    return `${HARNESS} has no credentials for ${model.provider}/${model.id}: ${String(error)}`;
  }
}

/**
 * Have the harness read its model files again in place, as its own `/model` does when it
 * opens ([`Context`]'s `modelRegistry.refresh`), for [`REREAD_MS`] at most: whether it
 * could be asked. A read that fails, or runs past the bound, leaves the catalog as far as
 * it got, and the lookup after it says what it holds. Never throws.
 */
async function reread(ctx: Context): Promise<boolean> {
  const registry = ctx.modelRegistry;
  if (!registry?.refresh) return false;
  const { promise: late, resolve } = Promise.withResolvers<void>();
  const timer = setTimeout(resolve, REREAD_MS);
  try {
    const read = HARNESS === "pi" ? registry.refresh({ signal: AbortSignal.timeout(REREAD_MS) }) : registry.refresh();
    await Promise.race([Promise.resolve(read).catch(() => undefined), late]);
  } catch {
    // It threw as it was asked: the catalog is as it was.
  } finally {
    clearTimeout(timer);
  }
  return true;
}

/** The fields of the harness's events this reads; every other field is read for nothing. */
type Event = {
  toolName?: unknown;
  toolCallId?: unknown;
  /** pi: set on a call another tool made, a codemode script's among them. */
  parentToolCallId?: unknown;
  input?: unknown;
  isError?: unknown;
  /** A call's result: its text parts. */
  content?: unknown;
  stop_hook_active?: unknown;
  text?: unknown;
  source?: unknown;
  message?: unknown;
  prompt?: unknown;
  /** pi's `agent_before_settle`: how the run's last answer ended. */
  outcome?: unknown;
  /** omp's `session_stop`: the run's last answer, whose `stopReason` says how it ended. */
  last_assistant_message?: unknown;
  /** omp's `agent_end`: the run goes on by itself. */
  willContinue?: unknown;
  /**
   * pi's `session_start` and `session_shutdown`, omp's `session_switch`: why. omp's
   * `auto_compaction_start` and pi's `session_before_compact`: why it compacts.
   */
  reason?: unknown;
  /** omp's `session_switch`: the conversation it left. */
  previousSessionFile?: unknown;
  /** pi's `session_compact_failed`: the compaction was cancelled, not refused. */
  aborted?: unknown;
  /** pi's `ui_prompt_start`: which dialog waits on its person, and its title. */
  kind?: unknown;
  title?: unknown;
  /** `session_before_compact`: what the harness would compact, the branch, and who asked with what. */
  preparation?: unknown;
  branchEntries?: unknown;
  customInstructions?: unknown;
  /** `session_compact`: the entry it wrote. */
  compactionEntry?: unknown;
  /** `context`: the messages the model is handed on this call, a copy the handler may change. */
  messages?: unknown;
  /** `before_provider_request`: what goes to the model's provider, the harness's to send unless replaced. */
  payload?: unknown;
};

type Api = {
  on(event: string, handler: (event: Event, ctx: Context) => unknown): void;
  getAllTools(): unknown[];
  /** pi returns nothing; omp a promise that settles when the turn the words start ends. */
  sendUserMessage(words: string, options?: { deliverAs: string }): unknown;
  /** `false` where the model's provider has no credentials. */
  setModel?(model: Model): Promise<boolean>;
  getThinkingLevel?(): string | undefined;
  /** Clamped by the harness to what the model can do. */
  setThinkingLevel?(level: string): void;
  /** pi: the value a command-line flag of the extension's own was given (registered by the loader). */
  getFlag?(name: string): boolean | string | undefined;
  /** pi: an MCP server for this session, replacing one of the same name this extension registered. */
  registerMcpServer?(name: string, config: Record<string, unknown>): void;
  /** A custom entry written into the conversation's own transcript, never sent to its model. */
  appendEntry?(customType: string, data?: unknown): void;
  unregisterMcpServer?(name: string): void;
};

/**
 * What a copy answers an ask of the runtime's (§4.4): `ok` with what the op says, or
 * `refused` with one of the op's refusal words — `no-answer` (words handed to the harness
 * and not yet seen written into the conversation, so on their way in; a turn still running after a stop),
 * `refused` (the harness threw, or the copy takes no such ask), `model-missing`,
 * `selection-unsupported` (a harness call this copy lacks), `harness-busy`, `other-turn`
 * when a stop names a say whose turn is not the one running, or `ending` when the
 * conversation stopped here, or this copy moved to another, before anything was handed
 * over — and the harness's own line (`error`).
 */
type Said = { ok: Record<string, unknown> } | { refused: string; error?: string };

/** One frame of the door, either way: a JSON object with its kind in `t`. */
type Frame = { t: string; [field: string]: unknown };

// ── which tools are Steery's ─────────────────────────────────────────────────

/**
 * A tool's name as Steery's catalog spells it, or nothing where the tool is not Steery's.
 * Both harnesses name an MCP server's tools after the server: pi `mcp__<server>__<tool>`,
 * omp `mcp__<server>_<tool>` (and has spelled it with two underscores).
 */
function steerys(name: unknown): string | undefined {
  if (typeof name !== "string") return undefined;
  for (const prefix of ["mcp__steery__", "mcp__steery_"]) {
    if (name.startsWith(prefix)) return name.slice(prefix.length);
  }
  return undefined;
}

/**
 * Whether this session can call the tool every conversation is listed, the one its line of
 * work is told with. pi 1.0 keeps an MCP server's tools listed while the server is away and
 * connects again at the next call, so a listed tool is reached; but a server taken away
 * hides its tools and keeps them listed as `exposure: "hidden"`, which no call reaches, and
 * they stay so until a registration connects again. Hidden is not reached.
 */
function reached(pi: Api): boolean {
  try {
    return pi
      .getAllTools()
      .some(
        (tool) =>
          !!tool &&
          typeof tool === "object" &&
          "name" in tool &&
          steerys(tool.name) === "sessions_describe" &&
          (tool as { exposure?: unknown }).exposure !== "hidden",
      );
  } catch {
    return false;
  }
}

// ── moments ──────────────────────────────────────────────────────────────────

/**
 * How much of its context window the conversation has left, 0 to 1, where it is known.
 *
 * The harness's own figure is an estimate wherever it stops trusting the model's count:
 * pi, after any edit to the context, counts four characters to a token, and read 630,362
 * tokens for a conversation the model had counted at 1,000,920 — 63% used while the model
 * refused every request as too long. The model's own count stands until a compaction or a
 * branch summary replaces what it read ([`newest`]), so the larger of the two is used.
 */
function left(ctx: Context): number | undefined {
  try {
    const usage = ctx.getContextUsage?.();
    const window = usage?.contextWindow;
    if (typeof window !== "number" || !(window > 0)) return undefined;
    const estimate = typeof usage?.tokens === "number" ? usage.tokens : undefined;
    const model = newest(ctx.sessionManager.getBranch?.() ?? [])?.total;
    const tokens = estimate === undefined ? model : Math.max(estimate, model ?? 0);
    if (tokens === undefined) return undefined;
    return Math.min(1, Math.max(0, 1 - tokens / window));
  } catch {
    return undefined;
  }
}

/**
 * What the model counted for a response of the assistant's: the tokens it read (`read`),
 * all it read and wrote (`total`), and whether it wrote the cache's one-hour entries (omp's
 * `usage.cttl`). None for a failed or aborted response, or one that counted nothing.
 */
function countedOf(entry: Entry): { read: number; total: number; hour: boolean } | undefined {
  if (entry.type !== "message" || !entry.message || typeof entry.message !== "object") return undefined;
  const message = entry.message as { role?: unknown; stopReason?: unknown; usage?: Record<string, unknown> };
  if (message.role !== "assistant" || message.stopReason === "error" || message.stopReason === "aborted") return undefined;
  const usage = message.usage ?? {};
  const n = (key: string) => (typeof usage[key] === "number" ? (usage[key] as number) : 0);
  const read = n("input") + n("cacheRead") + n("cacheWrite");
  const total = n("totalTokens") || read + n("output");
  if (!(total > 0)) return undefined;
  const ttl = usage.cttl && typeof usage.cttl === "object" ? (usage.cttl as Record<string, unknown>) : {};
  return { read, total, hour: typeof ttl.ephemeral1h === "number" && ttl.ephemeral1h > 0 };
}

/**
 * The newest response of a branch while it is still what the conversation holds — none once
 * a compaction or a branch summary came after it — at its index, with all its model counted.
 */
function newest(branch: readonly unknown[]): { at: number; entry: Entry; total: number } | undefined {
  for (let at = branch.length - 1; at >= 0; at--) {
    const entry = branch[at];
    if (!entry || typeof entry !== "object" || !("type" in entry)) continue;
    const one = entry as Entry;
    if (one.type === "compaction" || one.type === "branch_summary") return undefined;
    const counted = countedOf(one);
    if (counted) return { at, entry: one, total: counted.total };
  }
  return undefined;
}

/** A branch entry as both harnesses write one, its fields as far as they are read here. */
type Entry = {
  id?: unknown;
  type?: unknown;
  timestamp?: unknown;
  message?: unknown;
  firstKeptEntryId?: unknown;
  summary?: unknown;
  /** A compaction's: what its writer kept beside it, Steery's ask id for its own. */
  details?: unknown;
};

function entriesOf(list: unknown): Entry[] {
  return Array.isArray(list) ? list.filter((one): one is Entry => !!one && typeof one === "object") : [];
}

/** When an entry was written, in milliseconds: both harnesses stamp it in ISO 8601. */
function writtenAt(entry: Entry | undefined): number | undefined {
  const at = typeof entry?.timestamp === "string" ? Date.parse(entry.timestamp) : Number.NaN;
  return Number.isFinite(at) ? at : undefined;
}

/** The characters of what a model reads of an entry, as pi counts them for its estimate. */
function chars(value: unknown): number {
  if (typeof value === "string") return value.length;
  if (Array.isArray(value)) return value.reduce((sum: number, one) => sum + chars(one), 0);
  if (!value || typeof value !== "object") return 0;
  let sum = 0;
  for (const [key, field] of Object.entries(value)) {
    if (key === "type" && field === "image") return IMAGE_CHARS;
    if (key === "arguments") sum += JSON.stringify(field)?.length ?? 0;
    else if (READ_FIELDS[key]) sum += chars(field);
  }
  return sum;
}

/**
 * Tokens to a character of what the model reads of a branch, as its own model counted them:
 * how much more each response read than the one before it in the same context, over the
 * characters of the entries between ([`chars`]). Code and tool output run near two
 * characters a token, and pi's four counted about half of what Steery's compactions forgot.
 * Where the branch's model counted no such growth, pi's four ([`CHARS_PER_TOKEN`]).
 */
function tokenRate(branch: Entry[]): number {
  let tokens = 0;
  let read = 0;
  let previous: { at: number; read: number } | undefined;
  for (let at = 0; at < branch.length; at++) {
    const entry = branch[at];
    if (entry.type === "compaction" || entry.type === "branch_summary") {
      previous = undefined;
      continue;
    }
    const counted = countedOf(entry);
    if (!counted || !(counted.read > 0)) continue;
    if (previous) {
      let between = 0;
      for (let one = previous.at; one < at; one++) between += chars(branch[one].message ?? branch[one]);
      const grew = counted.read - previous.read;
      if (grew > 0 && between > 0) {
        tokens += grew;
        read += between;
      }
    }
    previous = { at, read: counted.read };
  }
  return tokens > 0 ? tokens / read : 1 / CHARS_PER_TOKEN;
}

/**
 * Where Steery's compaction of a branch would cut ([`compact`]). The context starts at the
 * first entry the latest compaction kept, else at the first entry (`from`). The cut is the
 * last message of the person's or the assistant's — never a tool result — written at or
 * before `before`, after the latest compaction and the context's start; what it forgets is
 * every entry from the start to the cut but that compaction, at `rate` tokens a character
 * ([`tokenRate`]).
 */
function cutOf(branch: Entry[], before: number, rate: number): { from: number; cut?: Entry; forgot: number } {
  const latest = branch.findLastIndex((entry) => entry.type === "compaction");
  const kept = latest < 0 ? 0 : branch.findIndex((entry) => entry.id === branch[latest]?.firstKeptEntryId);
  const start = kept < 0 ? latest : kept;
  const from = writtenAt(branch[start]) ?? Date.now();
  for (let at = branch.length - 1; at > Math.max(latest, start); at--) {
    const entry = branch[at];
    const message = entry?.message;
    const role = message && typeof message === "object" && "role" in message ? message.role : undefined;
    const when = writtenAt(entry);
    if (entry?.type !== "message" || (role !== "user" && role !== "assistant") || when === undefined || when > before) {
      continue;
    }
    let read = 0;
    for (const one of branch.slice(start, at)) read += one.type === "compaction" ? 0 : chars(one.message ?? one);
    return { from, cut: entry, forgot: Math.ceil(read * rate) };
  }
  return { from, forgot: 0 };
}

/**
 * Whether forgetting `forgot` tokens for a summary of `summary` tokens pays for itself
 * ([`WARM_SHARE`]). Always where the provider caches nothing of the context: no response
 * since the latest compaction ([`newest`]), or the newest written longer ago than the cache
 * lives — an hour where a response of the context wrote one-hour entries, else five
 * minutes. Else only where it forgets at least that share of what the context holds after.
 */
function worth(branch: Entry[], forgot: number, summary: number, now: number): boolean {
  const last = newest(branch);
  const at = writtenAt(last?.entry);
  if (!last || at === undefined || now - at >= CACHED_HOUR_MS) return true;
  let cached = now - at < CACHED_MS;
  for (let one = last.at; !cached && one >= 0; one--) {
    const entry = branch[one];
    if (entry.type === "compaction" || entry.type === "branch_summary") break;
    cached = countedOf(entry)?.hour === true;
  }
  return !cached || forgot >= WARM_SHARE * (last.total - forgot + summary);
}

/**
 * The messages a call hands `model`, opened with [`OPENING`] and its answer where they start
 * with a compaction's summary; else nothing, and the call goes as it was.
 */
function opened(messages: unknown, model: Model | undefined): unknown[] | undefined {
  if (!Array.isArray(messages) || !model?.api) return undefined;
  const first: unknown = messages[0];
  if (!first || typeof first !== "object" || !("role" in first) || first.role !== "compactionSummary") return undefined;
  const timestamp = "timestamp" in first && typeof first.timestamp === "number" ? first.timestamp : Date.now();
  const cost = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 };
  const usage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost };
  return [
    { role: "user", content: [{ type: "text", text: OPENING }], timestamp },
    {
      role: "assistant",
      content: [{ type: "text", text: OPENED }],
      api: model.api,
      provider: model.provider,
      model: model.id,
      usage,
      stopReason: "stop",
      timestamp,
    },
    ...messages,
  ];
}

/**
 * A provider request with no cache breakpoint on its tools, else nothing where none carries
 * one. A request takes at most four; omp spends one on its tools, which every breakpoint
 * after them covers too. A proxy that cloaks it as Claude Code (CLIProxyAPI) adds its own and
 * then drops the earliest message's, the system prompt it moved into a turn ([`OPENING`]).
 */
function unanchored(payload: unknown): unknown {
  if (!payload || typeof payload !== "object" || !("tools" in payload) || !Array.isArray(payload.tools)) return undefined;
  const anchored = (tool: unknown): tool is { cache_control: unknown } =>
    !!tool && typeof tool === "object" && "cache_control" in tool;
  if (!payload.tools.some(anchored)) return undefined;
  const tools = payload.tools.map((tool: unknown) => {
    if (!anchored(tool)) return tool;
    const { cache_control: _, ...rest } = tool;
    return rest;
  });
  return { ...payload, tools };
}

/**
 * Whose moment this is: the conversation, and for a sub-agent omp filed inside another's
 * transcript — `<lead>/<AgentId>.jsonl`, nested at most eight deep — the lead, with the
 * sub-agent's own identifier beside it, as the probe reads it. A sub-agent the lead marker
 * says ([`LEAD`]) is named by the document's `lead`.
 */
function speaker(ctx: Context): { session_id: string; agent_id?: string } {
  const own = ctx.sessionManager.getSessionId();
  const file = ctx.sessionManager.getSessionFile?.();
  if (!file) return { session_id: own };
  let root = file;
  for (let depth = 0; depth < 8 && existsSync(`${dirname(root)}.jsonl`); depth++) {
    root = `${dirname(root)}.jsonl`;
  }
  const stem = basename(root, ".jsonl");
  const cut = stem.lastIndexOf("_");
  if (root === file || cut < 0) return { session_id: own };
  return { session_id: stem.slice(cut + 1), agent_id: own };
}

/**
 * The variable naming the conversation that leads every process started from it,
 * `<harness>:<session id>`: the runtime reads a harness process that inherited one naming
 * another conversation as that one's sub-agent, and files its Steery calls under it.
 */
const LEAD = "STEERY_LEAD";

/**
 * The variable naming a file that holds a `compact` ask, exactly as the door carries one,
 * for a turn the runtime starts: the door opens only as its prompt runs, and the ask must be
 * answered before. The answer goes to `compacted.json` beside it (PROTOCOL.md §4.4).
 */
const COMPACT_ASK = "STEERY_COMPACT";

/**
 * The lead this process inherited from whatever started it, read before any session here
 * says one. The first copy to read it keeps it on the process: a copy that takes over from
 * an earlier one reads what the process inherited, not what that one's lead set.
 */
const INHERITED: string | undefined = inherited();

function inherited(): string | undefined {
  const shared = globalThis as unknown as Record<symbol, { lead?: string } | undefined>;
  const key = Symbol.for("steery.inherited");
  shared[key] ??= { lead: process.env[LEAD] || undefined };
  return shared[key].lead;
}

/**
 * The session in this process that leads it: the first one no other leads. omp runs its
 * sub-agents in the same process, and every one of them is led by it.
 */
let leading: object | undefined;

/** What the lead marker says of the conversation open in `ctx`. */
function marker(ctx: Context): string {
  return `${HARNESS}:${ctx.sessionManager.getSessionId()}`;
}

/** A conversation as Steery names one across harnesses. */
type Named = { harness: string; external_id: string };

/** The conversation the lead marker of this process names, where it names one other than `ctx`'s. */
function marked(ctx: Context): Named | undefined {
  const said = process.env[LEAD];
  if (!said || said === marker(ctx)) return undefined;
  const cut = said.indexOf(":");
  if (cut <= 0 || cut === said.length - 1) return undefined;
  return { harness: said.slice(0, cut), external_id: said.slice(cut + 1) };
}

/**
 * The conversation leading the one open in `ctx`: the one omp filed it inside, else the
 * one the lead marker names.
 */
function leadOf(ctx: Context): Named | undefined {
  const who = speaker(ctx);
  if (who.agent_id !== undefined) return { harness: HARNESS, external_id: who.session_id };
  return marked(ctx);
}

/**
 * The hook document the driver's `hook/read` reads for a moment, in the fields every
 * harness's document shares, stamped with when it happened (`at`, milliseconds since 1970):
 * a moment kept while no door was open reaches the runtime late, and is placed when it
 * happened. It also names the conversation's file, `transcript_path` as Claude Code names
 * it, for any session that is not a sub-agent, pi's or omp's: both keep their
 * conversations wherever a flag, a project setting or a variable of their person's shell
 * says, and the runtime sees none of those. A sub-agent's does not: its document names its
 * lead, and its own file would be another conversation's. The lead the process inherited
 * is said once, in the door's `hello`, never here.
 */
function document(ctx: Context, extra: Record<string, unknown> = {}): Record<string, unknown> {
  // Naming the sub-agent is how the runtime knows the conversation is its lead's.
  const who = speaker(ctx);
  const context_left = left(ctx);
  const transcript_path = who.agent_id === undefined ? ctx.sessionManager.getSessionFile?.() : undefined;
  return {
    ...who,
    cwd: ctx.cwd,
    ...(context_left === undefined ? {} : { context_left }),
    ...(typeof transcript_path === "string" && transcript_path ? { transcript_path } : {}),
    at: Date.now(),
    ...extra,
  };
}

/** A document written as a call's arguments, or nothing where it is not one. */
function parsed(text: unknown): unknown {
  if (typeof text !== "string") return undefined;
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

/**
 * The tool a call reaches and the arguments it passes, as Steery spells the tool. A
 * harness calls a tool directly by its name, or — where omp mounts tools behind paths —
 * by writing its arguments, as JSON, to `xd://<name>`.
 */
function called(event: Event): { tool: string | undefined; args: unknown } {
  const input = event.input && typeof event.input === "object" ? event.input : {};
  const path = "path" in input ? input.path : undefined;
  if (event.toolName === "write" && typeof path === "string" && path.startsWith("xd://")) {
    return {
      tool: steerys(path.slice("xd://".length)),
      args: parsed("content" in input ? input.content : undefined),
    };
  }
  return { tool: steerys(event.toolName), args: input };
}

/** The shell command a call runs, where it runs one: a `bash` tool's `command`, in both harnesses. */
function commandOf(input: unknown): string | undefined {
  const command = input && typeof input === "object" && "command" in input ? input.command : undefined;
  return typeof command === "string" && command.trim() ? command : undefined;
}

/** A Steery operation a shell command runs through the `steery` command line, and the words after it. */
type Ran = { op: string; words: string };

/**
 * The Steery operation a shell command runs, as the catalog names it (`requests.create`):
 * `steery <resource> <verb> …`, or a resource's singular for its `create` (`steery request
 * …`), by its path or its name alone. Nothing where the command runs no `steery`.
 */
function ran(command: string | undefined): Ran | undefined {
  const found = command?.match(/(?:^|[\s;&|(`'"\/])steery\s+([a-z][a-z_-]*)(?:\s+([a-z][a-z_-]*))?([^;&|\n]*)/);
  if (!found) return undefined;
  const [, resource, verb = "", rest = ""] = found;
  if (resource === "request") return { op: "requests.create", words: `${verb} ${rest}` };
  return { op: `${resource}.${verb}`, words: rest };
}

/**
 * Whether a Steery command that came back leaves its turn waiting rather than idle
 * (specs/RESOURCES.md §9): a `steery resources get` that waits, a `steery resources act`
 * `watch`, or one with `--queue` whose row says it joined the queue (`row.params.queue`).
 * Its turn's end is then not reminded: the wait, the watch or the queue wakes it.
 */
function waits(command: Ran, content: unknown): boolean {
  const { op, words } = command;
  if (op === "resources.get") return /--wait\b|"wait_secs"\s*:\s*[1-9]/.test(words);
  if (op !== "resources.act") return false;
  if (/--action[=\s]+["']?watch\b|"action"\s*:\s*"watch"/.test(words)) return true;
  if (!/--queue\b|"queue"\s*:\s*true/.test(words) || !Array.isArray(content)) return false;
  const text = (content as unknown[]).find(
    (part): part is { text: string } =>
      !!part && typeof part === "object" && "text" in part && typeof part.text === "string",
  )?.text;
  const acted = parsed(text?.slice(text.indexOf("{"), text.lastIndexOf("}") + 1));
  const row = acted && typeof acted === "object" && "row" in acted ? acted.row : undefined;
  const params = row && typeof row === "object" && "params" in row ? row.params : undefined;
  return !!params && typeof params === "object" && "queue" in params && params.queue === true;
}

/**
 * The signal ending this pi process, once one arrived. pi says `quit` for its person's
 * own `/quit`, Ctrl+C and Ctrl+D and for a signal alike; this is what tells them apart.
 *
 * Listened for once per process, from the first session's start. pi's interactive mode
 * listens for both signals itself and shuts down; a run that does not would be kept
 * alive by a listener of Steery's alone, so that one steps aside and raises the signal
 * again, and the process ends as it would have without this file.
 */
function signals(): { by?: string } {
  const shared = globalThis as unknown as Record<symbol, { by?: string } | undefined>;
  const key = Symbol.for("steery.signalled");
  const known = shared[key];
  if (known) return known;
  const state: { by?: string } = {};
  shared[key] = state;
  for (const signal of ["SIGTERM", "SIGHUP"] as const) {
    const heard = () => {
      state.by = signal;
      if (process.listenerCount(signal) === 1) {
        process.off(signal, heard);
        process.kill(process.pid, signal);
      }
    };
    process.on(signal, heard);
  }
  return state;
}

/**
 * Why this conversation is closing, in the harness's own word, for the runtime to judge
 * whether its person closed it ([`Harness::closes`]). pi says it on the event. omp says
 * nothing there, but writes its reason down as the session's last entry just before: the
 * `session_exit` it records, `dispose` for a keypress or `/exit`, a signal's name for a
 * signal. An older entry of that name is an earlier run's, so only the last entry is read.
 */
function closing(event: Event, ctx: Context): unknown {
  if (HARNESS === "pi") return event.reason;
  const entries = ctx.sessionManager.getEntries?.() ?? [];
  const last: unknown = entries[entries.length - 1];
  if (!last || typeof last !== "object" || !("type" in last) || last.type !== "custom") return undefined;
  if (!("customType" in last) || last.customType !== "session_exit" || !("data" in last)) return undefined;
  const data = last.data;
  return data && typeof data === "object" && "reason" in data ? data.reason : undefined;
}

/**
 * One moment for the runtime, by an id unique on this machine (`<pid>.<nonce>.<n>`, the
 * nonce drawn once per process, so a recycled pid resuming the same conversation never
 * repeats an id): what its answer comes back under, the same however often it is sent, so
 * the runtime reads a moment sent again once. `event` is the hook event it is.
 */
type Moment = { id: string; event: string; document: Record<string, unknown> };

/**
 * What the process keeps of its moments: the ones no door has carried yet, oldest first;
 * the next id; and what waits for an answer, by id. A moment said while the runtime is
 * away, or as a copy hands over to the next — a `/reload`, a session replaced, a newer
 * copy — goes with the next door any copy here opens, in the order it happened, and its
 * answer reaches the copy that asked whichever door carried it. Every copy in a process
 * is the same harness's, and a moment's document names its own conversation.
 */
type Moments = {
  queue: Moment[];
  nonce: string;
  next: number;
  waiting: Map<string, (frame: Frame | undefined) => void>;
};

function moments(): Moments {
  const shared = globalThis as unknown as Record<symbol, Moments | undefined>;
  const key = Symbol.for("steery.moments");
  shared[key] ??= {
    queue: [],
    nonce: Math.random().toString(36).slice(2, 10),
    next: 0,
    waiting: new Map(),
  };
  return shared[key];
}

/** A fresh id, unique on this machine, for a frame it sends and awaits an answer to. */
function nextId(): string {
  const kept = moments();
  kept.next += 1;
  return `${process.pid}.${kept.nonce}.${kept.next}`;
}

/** Keep moments for the next door, oldest dropped first past the bound. */
function keep(...said: Moment[]): void {
  const queue = moments().queue;
  queue.push(...said);
  if (queue.length > MOMENTS_KEPT) queue.splice(0, queue.length - MOMENTS_KEPT);
}

/**
 * The runtime's asks this conversation answered, by id, kept on the process by the
 * conversation's id: an ask sent again — on this door, or on the next after a reconnect,
 * a `/reload` or a newer copy — is answered from the first, and what it did is never done
 * twice.
 */
function answeredIn(session: string): Map<string, Promise<Said>> {
  const shared = globalThis as unknown as Record<symbol, Map<string, Map<string, Promise<Said>>> | undefined>;
  const key = Symbol.for("steery.answered");
  shared[key] ??= new Map();
  const all = shared[key];
  let one = all.get(session);
  if (!one) {
    one = new Map();
    all.set(session, one);
  }
  return one;
}

/** Forget what a conversation that ended here answered. */
function forgotten(session: string): void {
  const shared = globalThis as unknown as Record<symbol, Map<string, Map<string, Promise<Said>>> | undefined>;
  shared[Symbol.for("steery.answered")]?.delete(session);
}

// ── the door ─────────────────────────────────────────────────────────────────

/** The one-shot runs, whose process ends with the turn it was started for. */
function oneShot(ctx: Context): boolean {
  return ctx.mode === "print" || ctx.mode === "json";
}

/** A user message's words, as the text the harness took. */
function spoken(message: unknown): string | undefined {
  if (!message || typeof message !== "object" || !("role" in message) || message.role !== "user") return undefined;
  const content = "content" in message ? message.content : undefined;
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return undefined;
  return content
    .flatMap((part: unknown) =>
      part && typeof part === "object" && "type" in part && part.type === "text" && "text" in part ? [part.text] : [],
    )
    .join("\n");
}

/**
 * Steery's own words put in through the door, kept by the say's id until they go into the
 * transcript: the throw whose words they are, where an aim's, and the ids of the later
 * says of that throw answered from these words rather than handed again (`also`).
 */
type Say = { id: string; throw?: string; also: string[]; words: string; mode: "steer" | "queue"; seen: boolean };

/** A call the conversation's agent is inside, with the calls made from inside it that are still running: LiveState's. */
type Call = { id: string; tool: string; since: number; children: { id: string; tool: string }[] };

/** A call the conversation's agent was inside that ended, and when, by this process's clock. */
type Ended = { id: string; at: number };

/** The kinds of dialog LiveState names; any other of the harness's is `custom`. */
const DIALOG_KINDS = ["select", "confirm", "input", "editor", "custom"] as const;

/** A dialog of the harness's own waiting on its person. */
type Dialog = { kind: (typeof DIALOG_KINDS)[number]; title: string; since: number };

/**
 * A turn Steery's words began that ended, by this process's clock: the says it took as
 * Steery's, and whether its last answer stopped on an error it did not get past, with the
 * harness's line for it, where it gave one.
 */
type Settled = { began_at: number; at: number; says: string[]; failed: boolean; error?: string };

/**
 * Where a session stands in its turn, as one copy hands it to the copy that takes the
 * session over: read field by field, so a field this copy does not know is left behind
 * and one it lacks starts as at a session's start.
 */
type Kept = {
  asked: boolean;
  parked: boolean;
  /** The turn's last Steery call was a wait: on a resource, a watch, or a place in a queue. */
  waiting: boolean;
  reminded: boolean;
  turning: boolean;
  began: number | undefined;
  retrying: boolean;
  continued: string | undefined;
  says: Say[];
  ours: string[];
  /** The says the turn running now took as Steery's, whatever came into it since. */
  turnSays: string[];
  /** The harness's line for the error the turn running now last stopped on, while it is not past it. */
  failure: string | undefined;
  settled: Settled[];
  calls: Call[];
  ended: Ended[];
  dialog: Dialog | null;
};

/**
/**
 * What the loader hands a copy as it starts: the context the harness last handed it where
 * it takes over a session already under way, what the copy before it kept, and the
 * harness's own model calls ([`Models`]).
 */
type Carried = { ctx?: Context; kept?: unknown; models?: unknown };

/**
 * What of the harness's own packages a recap needs ([`recap`]), as the loader imports them:
 * `convertToLlm` from its coding agent, and — for omp, whose registry makes no request
 * itself — `completeSimple` from its model package.
 */
type Models = {
  convertToLlm?(messages: unknown[]): unknown[];
  completeSimple?(model: Model, context: unknown, options: Record<string, unknown>): Promise<Answered>;
};

/**
 * A copy running in one session: stopped, it lets go of all it holds and says where the
 * session stands.
 */
type Running = { stop(): Promise<Kept> };

function restored(kept: unknown): Kept {
  const field = (name: string): unknown =>
    kept && typeof kept === "object" && name in kept ? (kept as Record<string, unknown>)[name] : undefined;
  const says = field("says");
  const ours = field("ours");
  const turnSays = field("turnSays");
  const failure = field("failure");
  const settled = field("settled");
  const continued = field("continued");
  const calls = field("calls");
  const ended = field("ended");
  const dialog = field("dialog");
  const turning = field("turning") === true;
  const began = field("began");
  return {
    asked: field("asked") === true,
    parked: field("parked") === true,
    waiting: field("waiting") === true,
    reminded: field("reminded") === true,
    turning,
    began: typeof began === "number" ? began : undefined,
    retrying: field("retrying") === true,
    continued: typeof continued === "string" ? continued : undefined,
    says: Array.isArray(says) ? says.filter(isSay) : [],
    ours: Array.isArray(ours) ? ours.filter((one): one is string => typeof one === "string") : [],
    turnSays: Array.isArray(turnSays) ? turnSays.filter((one): one is string => typeof one === "string") : [],
    failure: typeof failure === "string" ? failure : undefined,
    settled: Array.isArray(settled) ? settled.filter(isSettled).slice(-SETTLED_KEPT) : [],
    calls: Array.isArray(calls) ? calls.filter(isCall) : [],
    ended: Array.isArray(ended) ? ended.filter(isEnded).slice(-ENDED_KEPT) : [],
    dialog: isDialog(dialog) ? dialog : null,
  };
}

function isSay(one: unknown): one is Say {
  if (!one || typeof one !== "object") return false;
  const say = one as Record<string, unknown>;
  return (
    typeof say.id === "string" &&
    (say.throw === undefined || typeof say.throw === "string") &&
    Array.isArray(say.also) &&
    say.also.every((one) => typeof one === "string") &&
    typeof say.words === "string" &&
    (say.mode === "steer" || say.mode === "queue") &&
    typeof say.seen === "boolean"
  );
}

/** Every say id a [`Say`]'s words answer for: its own, then those answered from it. */
function idsOf(say: Say): string[] {
  return [say.id, ...say.also];
}

function isCall(one: unknown): one is Call {
  if (!one || typeof one !== "object") return false;
  const call = one as Record<string, unknown>;
  return (
    typeof call.id === "string" &&
    typeof call.tool === "string" &&
    typeof call.since === "number" &&
    Array.isArray(call.children) &&
    call.children.every(
      (child: unknown) =>
        !!child &&
        typeof child === "object" &&
        "id" in child &&
        typeof child.id === "string" &&
        "tool" in child &&
        typeof child.tool === "string",
    )
  );
}

function isEnded(one: unknown): one is Ended {
  if (!one || typeof one !== "object") return false;
  const ended = one as Record<string, unknown>;
  return typeof ended.id === "string" && typeof ended.at === "number";
}

function isSettled(one: unknown): one is Settled {
  if (!one || typeof one !== "object") return false;
  const settled = one as Record<string, unknown>;
  return (
    typeof settled.began_at === "number" &&
    typeof settled.at === "number" &&
    Array.isArray(settled.says) &&
    settled.says.every((say) => typeof say === "string") &&
    typeof settled.failed === "boolean" &&
    (settled.error === undefined || typeof settled.error === "string")
  );
}

function isDialog(one: unknown): one is Dialog {
  if (!one || typeof one !== "object") return false;
  const dialog = one as Record<string, unknown>;
  return (
    DIALOG_KINDS.some((kind) => kind === dialog.kind) &&
    typeof dialog.title === "string" &&
    typeof dialog.since === "number"
  );
}

/**
 * Where each session pi is reloading stood, by its id. pi's `/reload` tears this copy down
 * and builds another over the same conversation, and the loader that would carry one over
 * is built anew with it; so the copy going leaves it on the process, and the copy that
 * starts next takes it, once.
 */
function reloading(): Map<string, Kept> {
  const shared = globalThis as unknown as Record<symbol, Map<string, Kept> | undefined>;
  const key = Symbol.for("steery.reloading");
  shared[key] ??= new Map();
  return shared[key];
}

function str(value: unknown): string {
  return typeof value === "string" ? value : "";
}

export const surfaces = {
  // The pick a run the runtime starts was given, beside pi's own `--model` and
  // `--thinking`, which pi may resolve to something else: checked as the run's words
  // come in (`input`, below). pi refuses a flag nobody registered before anything
  // starts, which is the runtime's sign this copy is absent. omp is given none: the
  // runtime checks a pick against omp's own catalog before the run starts.
  flags:
    HARNESS === "pi"
      ? [
          { name: "steery-model", description: "The model Steery's runtime picked", type: "string" as const },
          { name: "steery-thinking", description: "The thinking Steery's runtime picked", type: "string" as const },
        ]
      : [],
};

export default function steery(pi: Api, carried: Carried = {}): Running {
  // Per conversation. pi builds this again for every session it replaces the last one
  // with and at every `/reload` ([`reloading`]), and omp for every session it runs, a
  // sub-agent's included; the loader builds it again for a session already under way,
  // carrying on from what the copy before kept.
  const kept = restored(carried.kept);
  // Whether this turn has asked its person anything, whether its agent parked the
  // conversation, whether its last Steery call was a wait (on a resource, a watch, a place
  // in a queue: waiting is not idling), and whether it was already reminded: each begins
  // again at every turn the person starts.
  let asked = kept.asked;
  let parked = kept.parked;
  let waiting = kept.waiting;
  let reminded = kept.reminded;
  // The context the harness last handed a handler: the one the door speaks through.
  let live: Context | undefined;
  // omp mutates its SessionManager in place before announcing a switch. Keep the
  // identity this copy took up, not another read through that mutable object.
  let sessionId: string | undefined;
  const inSession = (id: string | undefined): boolean =>
    id !== undefined && sessionId === id && live?.sessionManager.getSessionId() === id;
  const self = {};

  /**
   * Words handed over through the door, until the harness is seen taking them: the throw
   * whose words they are, where an aim's, and the answer they will be said with.
   */
  const pending: {
    words: string;
    throw: string | undefined;
    handed: boolean;
    said: Promise<Said>;
    answer(said: Said): void;
  }[] = [];
  // A turn the harness is preparing — from `before_agent_start` to `agent_start` — reads
  // as idle, and words handed over then would start a turn of their own beside it: they
  // wait until it runs, and go in as words steered into it.
  let starting: PromiseWithResolvers<void> | undefined;
  const begun = () => {
    starting?.resolve();
    starting = undefined;
  };
  // pi refuses any words while it compacts — before its `input` event, and through this
  // API silently — so words handed over then would be lost: they wait until it is done,
  // and are refused `harness-busy` if it is not done in time ([`HEARD_WITHIN_MS`]). Words
  // wait the same while Steery's own compaction runs in either harness ([`compact`]).
  let compacting: PromiseWithResolvers<void> | undefined;
  const compacted = () => {
    compacting?.resolve();
    compacting = undefined;
  };
  /**
   * Steery's own compaction while its ask waits on the harness ([`compact`]): the ask, the
   * marker its `customInstructions` carry, the summary and cut the hook hands the harness,
   * and whether the harness wrote it.
   */
  let ownCompaction: { id: string; marker: string; summary: string; cut: string; seen: boolean } | undefined;
  /**
   * omp: why it compacts on its own (`threshold`, `overflow`, `idle`, `incomplete`), from
   * its `auto_compaction_start` to its `auto_compaction_end`. omp says no reason at its
   * `session_before_compact`; every compaction it starts on its own and offers that hook
   * is announced first (v18.10.4 `session-maintenance.ts`), so one not announced was asked
   * for: `/compact`, rpc `compact`, or an extension's `compact`, as pi says `manual` too.
   */
  let autoCompaction: { reason?: string } | undefined;
  /**
   * The cause last written ([`COMPACTION`]), until a compaction or a message is written
   * after it: omp offers `session_before_compact` again for each method it tries
   * (v18.10.4), and the entry already stands before the compaction to come.
   */
  let causeWritten: string | undefined;

  /**
   * The harness wrote these words into the conversation as the person's message: the ones
   * handed over are in. The only proof a say is answered `ok` on ([`deliver`]); a say
   * already answered `no-answer` is remembered `ok` from now ([`addressed`]).
   */
  const taken = (text: unknown) => {
    if (typeof text !== "string") return;
    const words = text.trim();
    for (const one of pending.filter((one) => one.handed && one.words === words)) one.answer({ ok: {} });
    if (writing?.words === words && sessionId !== undefined) {
      const answered = answeredIn(sessionId);
      for (const id of idsOf(writing)) if (answered.has(id)) answered.set(id, Promise.resolve({ ok: {} }));
    }
    writing = undefined;
  };
  /** The say whose words the harness is writing now, from `message_start` to `message_end`. */
  let writing: Say | undefined;

  /**
   * Steery's own words, put in through `say` and kept by the say's id until the harness is
   * seen taking them as input (`seen` once a batch held them, dropped once their message
   * goes into the transcript): what tells a turn Steery started from one its person did.
   */
  const says: Say[] = kept.says;
  /**
   * The says whose words began the stretch of the turn running now: none where its person
   * started it, or put words of their own into it. The one turn a `stop` may stop is the
   * stretch of the say it names.
   */
  let ours = new Set<string>(kept.ours);
  /**
   * The says the turn running now took as Steery's — that began it, or went on with it
   * while it was Steery's — whatever came into it since: the turn each of them began, as
   * the door's `state` names it, and the run its aim reports.
   */
  let turnSays = new Set<string>(kept.turnSays);
  /** A turn began, and the input it began with is still to be read. */
  let opening = false;
  /** The reminder this conversation's stop hook last asked omp to go on with. */
  let continued: string | undefined = kept.continued;

  /** Steery's says among a batch of input, and whether anything else is in it: the person's. */
  const whose = (batch: string): { mine: Say[]; theirs: boolean } => {
    let rest = `\n\n${batch.trim()}\n\n`;
    const mine = says.filter((one) => rest.includes(`\n\n${one.words}\n\n`));
    for (const one of mine) rest = rest.replace(`\n\n${one.words}\n\n`, "\n\n");
    return { mine, theirs: rest.trim() !== "" };
  };

  /**
   * Input the harness is taking now, and so whose the turn's stretch is: words of the
   * person's make it theirs; Steery's words that open a turn, or follow one that has done
   * its work (`queue`), begin a stretch of Steery's; Steery's words steered into a turn
   * join whoever's it is, so a turn of the person's never becomes Steery's to stop.
   */
  const heard = ({ mine, theirs }: { mine: Say[]; theirs: boolean }) => {
    const began = opening;
    opening = false;
    for (const one of mine) one.seen = true;
    if (theirs) ours = new Set();
    else if (began || mine.some((one) => one.mode === "queue")) ours = new Set(mine.flatMap(idsOf));
    else if (ours.size > 0) for (const id of mine.flatMap(idsOf)) ours.add(id);
    for (const id of ours) turnSays.add(id);
  };

  // Inside a turn from `before_agent_start` until its run is over (`agent_settled`, or an
  // `agent_end` that does not go on by itself): the harness reads as idle while it
  // prepares one, and between the runs it goes on with, and the door must not.
  let turning = kept.turning;
  /** When the turn running now began. */
  let began = kept.began;
  /**
   * The harness's line for the error the turn running now last stopped on, `""` where it
   * gave none, until an answer gets past it: the turn failed where it ends so.
   */
  let failure: string | undefined = kept.failure;
  /** How the turn's last answer stopped, as the harness said it: what its `Stop` document carries. */
  let stopped: { stop_reason?: string; error_message?: string } | undefined;
  /** A settle's answer kept the turn running now going once: what its `Stop` document says (`continued`). */
  let keptGoing = false;
  /** Steery's turns that ended lately, oldest first, at most [`SETTLED_KEPT`]. */
  const settled: Settled[] = kept.settled;
  // omp waits between its own retries after `agent_end` and reads as idle meanwhile; it
  // goes on by itself (`auto_retry_start` to `auto_retry_end`). pi's run is not over
  // until `agent_settled`, retries included.
  let retrying = kept.retrying;
  /** pi: this run's overflow recovery failed, so it settles with its context full. */
  let overflowed = false;
  /** The calls the agent is inside, by id, oldest first: what the conversation waits on. */
  const calls = new Map<string, Call>(kept.calls.map((call) => [call.id, call]));
  /** The calls it was inside that ended lately, oldest first, at most [`ENDED_KEPT`]. */
  const ended: Ended[] = kept.ended;
  /** A dialog of the harness's own waiting on its person. */
  let dialog: Dialog | null = kept.dialog;
  /** The harness's own model calls, as the loader handed them: without them no recap is written here. */
  const models = carried.models && typeof carried.models === "object" ? (carried.models as Models) : undefined;

  /**
   * Whether the conversation can take words with no turn running and none waiting to go
   * in; unknown from a harness without the two questions.
   */
  const idle = (ctx: Context): boolean | undefined => {
    if (turning || retrying) return false;
    const running = ctx.isIdle?.();
    const queued = ctx.hasPendingMessages?.();
    if (running === undefined || queued === undefined) return undefined;
    return running && !queued;
  };

  /**
   * Whether somebody is typing at the conversation's input line: it holds words. A
   * session nobody can type into has nobody typing; one that cannot say is unknown.
   */
  const typing = (ctx: Context): boolean | undefined => {
    if (ctx.hasUI === false) return false;
    try {
      const text = ctx.ui?.getEditorText?.();
      return typeof text === "string" ? text.trim() !== "" : undefined;
    } catch {
      return undefined;
    }
  };

  /**
   * Whether words that may go in only while nothing else would — Steery's own nobody
   * asked for now, another member's, a reviewed answer — may go in now: no turn, nothing
   * queued, nobody typing at the input line, no turn being prepared and no compaction,
   * read as they are handed over.
   */
  const quiet = (ctx: Context): boolean => idle(ctx) === true && typing(ctx) === false && !starting && !compacting;

  /**
   * The session's model as Steery names it, `<provider>/<id>`, and how hard it thinks: a
   * pick's answer as read back. A field the harness does not say is absent.
   */
  const current = (ctx: Context): { model?: string; thinking?: string } => {
    const model = ctx.models?.current?.() ?? ctx.model;
    let thinking: string | undefined;
    try {
      thinking = pi.getThinkingLevel?.() ?? undefined;
    } catch {
      // A harness not yet running its actions says nothing of it.
    }
    return {
      ...(model ? { model: `${model.provider}/${model.id}` } : {}),
      ...(thinking === undefined ? {} : { thinking }),
    };
  };

  /** What the harness runs past a turn now; nothing from one that cannot say. */
  const snapshot = (ctx: Context) => {
    try {
      return ctx.getAsyncJobSnapshot?.();
    } catch {
      return undefined;
    }
  };

  /**
   * How much background work runs in this process past its turns: the jobs running, and
   * their results waiting to be told or being told. Ended with the process, so a process
   * with any is never ended for being idle (LiveState `jobs`).
   */
  const jobs = (ctx: Context): number => {
    const now = snapshot(ctx);
    return (
      (Array.isArray(now?.running) ? now.running.length : 0) +
      (now?.delivery?.queued ?? 0) +
      (now?.delivery?.delivering ? 1 : 0)
    );
  };

  /** What the conversation is doing now: a LiveState (§4.3), its unknowns absent. */
  const stateNow = (ctx: Context): Record<string, unknown> => {
    const known = (field: string, value: boolean | number | undefined) =>
      value === undefined ? {} : { [field]: value };
    const running = jobs(ctx);
    return {
      ...(turning && began !== undefined ? { turn: { began_at: began, says: [...turnSays] } } : {}),
      ...known("idle", idle(ctx)),
      ...known("pending", ctx.hasPendingMessages?.()),
      ...known("typing", typing(ctx)),
      compacting: compacting !== undefined,
      retrying,
      ...(running > 0 ? { jobs: running } : {}),
      ...current(ctx),
      ...known("context_left", left(ctx)),
      calls: [...calls.values()],
      ended,
      ...(dialog ? { dialog } : {}),
      settled,
    };
  };

  // The door: one WebSocket to the runtime, dialed again whenever it drops, for as long as
  // this copy holds the conversation here (`wanted`).
  let door: WebSocket | undefined;
  /** The runtime accepted this copy's `hello` on the door open now. */
  let welcomed = false;
  let wanted = false;
  /** This copy is letting go: the runtime's asks are refused `ending`. */
  let ending = false;
  /** The runtime could not read this copy's hello (4400): it dials no more until the harness starts it again. */
  let unheard = false;
  let redial: NodeJS.Timeout | undefined;
  let backoff = REDIAL_MIN_MS;
  /** A dial failed before the runtime welcomed one: pi's own connection to Steery's tools may have failed too. */
  let missed = false;
  /** Moments sent on the door open now that the runtime has not answered. */
  const inflight = new Map<string, Moment>();
  /**
   * This copy's settles still waiting for the runtime: answered nothing when the door
   * drops. A moment's waiter is the process's ([`moments`]): the moment goes again on the
   * next door, and its answer may yet arrive within its bound.
   */
  const settling = new Set<string>();
  /** The `state` last sent, when, and the one going out next. */
  let stateSent = "";
  let stateAt = 0;
  let stateTimer: NodeJS.Timeout | undefined;
  let looking: NodeJS.Timeout | undefined;

  const send = (frame: Frame): boolean => {
    if (!door || door.readyState !== 1) return false;
    try {
      door.send(JSON.stringify(frame));
      return true;
    } catch {
      return false;
    }
  };

  /**
   * The runtime's answer to the frame sent under `id`, or nothing once `ms` passed. Nothing
   * at once where no door is open or opening.
   */
  const awaited = (id: string, ms: number): Promise<Frame | undefined> => {
    if (!door) return Promise.resolve(undefined);
    const waiting = moments().waiting;
    const { promise, resolve } = Promise.withResolvers<Frame | undefined>();
    const timer = setTimeout(() => {
      waiting.delete(id);
      resolve(undefined);
    }, ms);
    waiting.set(id, (frame) => {
      clearTimeout(timer);
      waiting.delete(id);
      resolve(frame);
    });
    return promise;
  };

  /** Send a moment on the door, or keep it for the next one. */
  const post = (one: Moment) => {
    if (welcomed && send({ t: "moment", id: one.id, event: one.event, document: one.document })) {
      inflight.set(one.id, one);
    } else {
      keep(one);
    }
  };

  /** Say one moment happened, its hook document as the harness gave it: its id is what its answer comes back under. */
  const moment = (event: string, doc: Record<string, unknown>): string => {
    const id = nextId();
    post({ id, event, document: doc });
    return id;
  };

  /** The words a moment's or a settle's answer has for the agent: `ok.say`, where it says any. */
  const sayOf = (answered: Frame | undefined): string | undefined => {
    const ok = answered?.ok;
    const say = ok && typeof ok === "object" && "say" in ok ? ok.say : undefined;
    return typeof say === "string" && say.trim() ? say : undefined;
  };

  /** Send what the conversation is doing, where it changed since it was last sent. */
  const sendState = () => {
    stateTimer = undefined;
    const ctx = live;
    if (!welcomed || !ctx) return;
    let state: string;
    try {
      state = JSON.stringify(stateNow(ctx));
    } catch {
      // A context the harness already retired says nothing more.
      return;
    }
    if (state === stateSent) return;
    if (door?.readyState !== 1) return;
    try {
      door.send(`{"t":"state","state":${state}}`);
    } catch {
      return;
    }
    stateSent = state;
    stateAt = Date.now();
  };

  /**
   * The conversation's transcript, held open for writing while this process is in it: the
   * kernel then names the process in its conversation even while its door is down. It
   * writes nothing; `O_RDWR` neither creates nor truncates. Close-on-exec: without it every
   * command the harness runs inherits the descriptor, and a background job that outlives
   * the turn goes on holding a transcript it is in no conversation of. pi writes a new
   * session's file only at its first message, so each look tries again until it is open; a
   * session that moves lets go of the old one first.
   */
  let held: { file: string; fd: number } | undefined;
  /** The session ended here or moved onto a newer copy: nothing is held again. */
  let unheld = false;
  const unhold = () => {
    if (!held) return;
    try {
      closeSync(held.fd);
    } catch {
      // Already closed: nothing is held.
    }
    held = undefined;
  };
  const hold = (ctx: Context | undefined) => {
    if (unheld || !ctx) return;
    let file: string | undefined;
    try {
      file = ctx.sessionManager.getSessionFile?.();
    } catch {
      return;
    }
    if (held?.file === file) return;
    unhold();
    if (!file) return;
    try {
      held = { file, fd: openSync(file, constants.O_RDWR | O_CLOEXEC) };
    } catch {
      // Not written yet, or unreadable: the next look tries again.
    }
  };

  /** Something the state says may have changed: sent within [`STATE_EVERY_MS`]. */
  const noteState = () => {
    hold(live);
    if (stateTimer || !welcomed) return;
    stateTimer = setTimeout(sendState, Math.max(0, stateAt + STATE_EVERY_MS - Date.now()));
    stateTimer.unref?.();
  };

  /**
   * pi: the headers that name the conversation on every call to Steery's tools: its own,
   * or for a sub-agent its lead's with its own id beside it. A sub-agent whose lead
   * nothing names says nobody, and its calls are filed as any unnamed caller's: it is
   * never named as a conversation of its own.
   */
  const callerHeaders = (ctx: Context): Record<string, string> => {
    const own = ctx.sessionManager.getSessionId();
    if (!led(ctx)) return { "Steery-Harness": HARNESS, "Steery-External-Id": own };
    const lead = leadOf(ctx);
    return lead
      ? { "Steery-Harness": lead.harness, "Steery-External-Id": lead.external_id, "Steery-Sub-Agent": own }
      : {};
  };

  /**
   * pi: Steery's MCP entry for this conversation, with headers naming it. pi connects it
   * as the session starts, and a registration the same as the one it holds changes
   * nothing. One pi refuses leaves the harness as it is without Steery.
   */
  const register = (ctx: Context) => {
    if (!REGISTERS || !pi.registerMcpServer) return;
    try {
      pi.registerMcpServer("steery", { ...MCP, headers: callerHeaders(ctx) });
    } catch {
      // The harness runs as without Steery's tools.
    }
  };

  /**
   * pi: connect Steery's tools again. A first connection that failed while the runtime
   * was away listed no tools, and one taken away hid them ([`reached`]): pi calls nothing
   * it does not list, so it never tries again by itself. A registration taken away and
   * given back connects at once. Given back a moment later, so pi sees two changes and
   * not one that changed nothing.
   */
  const reconnect = () => {
    if (!REGISTERS || !pi.unregisterMcpServer) return;
    try {
      pi.unregisterMcpServer("steery");
    } catch {
      return;
    }
    setTimeout(() => {
      if (live && !ending) register(live);
    }, 0);
  };

  /** The runtime accepted this copy: what was kept goes, then what the conversation is doing, again. */
  const welcome = () => {
    welcomed = true;
    backoff = REDIAL_MIN_MS;
    for (const one of moments().queue.splice(0)) post(one);
    stateSent = "";
    sendState();
    clearInterval(looking);
    looking = setInterval(noteState, LOOK_EVERY_MS);
    looking.unref?.();
    if (REGISTERS && missed && !reached(pi)) reconnect();
    missed = false;
  };

  /** The door open now is gone: what it had not answered goes with the next. */
  const detached = () => {
    door = undefined;
    welcomed = false;
    clearInterval(looking);
    looking = undefined;
    clearTimeout(stateTimer);
    stateTimer = undefined;
    stateSent = "";
    const unanswered = [...inflight.values()];
    inflight.clear();
    moments().queue.unshift(...unanswered);
    keep();
    const waiting = moments().waiting;
    for (const id of [...settling]) waiting.get(id)?.(undefined);
    settling.clear();
  };

  /** One frame from the runtime; one unreadable, or of a kind or a door this copy does not know, is dropped. */
  const arrived = (data: unknown) => {
    const frame = parsed(typeof data === "string" ? data : String(data));
    if (!frame || typeof frame !== "object" || !("t" in frame)) return;
    const said = frame as Frame;
    switch (said.t) {
      case "welcome":
        if (said.door === DOOR_VERSION && !welcomed) welcome();
        return;
      case "answer": {
        if (typeof said.id !== "string" || !said.ok || typeof said.ok !== "object") return;
        inflight.delete(said.id);
        moments().waiting.get(said.id)?.(said);
        return;
      }
      case "ask": {
        const id = said.id;
        if (typeof id !== "string" || !id || typeof said.op !== "string") return;
        void addressed(id, said).then((reply) => send({ t: "answer", id, ...reply }));
        return;
      }
      default:
        return;
    }
  };

  /** Dial the door, where this copy holds the conversation and no door is open or opening. */
  const dial = () => {
    clearTimeout(redial);
    redial = undefined;
    const ctx = live;
    if (!wanted || unheard || door || !ctx || typeof WebSocket === "undefined") return;
    let socket: WebSocket;
    try {
      socket = new WebSocket(DOOR);
    } catch {
      dropped(undefined);
      return;
    }
    door = socket;
    welcomed = false;
    socket.onopen = () => {
      if (door !== socket) return;
      const now = live ?? ctx;
      try {
        socket.send(JSON.stringify(hello(now)));
      } catch {
        socket.close();
      }
    };
    socket.onmessage = (message: MessageEvent) => {
      if (door === socket) arrived(message.data);
    };
    socket.onclose = (closed: CloseEvent) => {
      if (door === socket) dropped(closed.code);
    };
    socket.onerror = () => {
      // The close that follows says what became of it.
    };
  };

  /**
   * The door dropped: dial again after [`REDIAL_MIN_MS`], doubling to [`REDIAL_MAX_MS`],
   * from the least again once a door was welcomed; after [`OTHER_DOOR_MS`] where the runtime
   * speaks another door version; never where a newer door replaced this one, or the
   * runtime could not read this copy's hello.
   */
  const dropped = (code: number | undefined) => {
    const was = welcomed;
    detached();
    if (!was) missed = true;
    if (code === UNHEARD) unheard = true;
    if (!wanted || unheard || code === REPLACED) return;
    if (was) backoff = REDIAL_MIN_MS;
    const wait = code === OTHER_DOOR ? OTHER_DOOR_MS : backoff;
    backoff = Math.min(backoff * 2, REDIAL_MAX_MS);
    redial = setTimeout(dial, wait);
    redial.unref?.();
  };

  /** Close the door open now, its conversation ended here or moved, keeping what it had not answered for the next. */
  const cut = () => {
    const was = door;
    detached();
    try {
      was?.close(LET_GO);
    } catch {
      // Already closed.
    }
  };

  /** Who is at the harness, as the hello says it: `tui` or `rpc` where a person is, `json` or `print` for a one-shot run. */
  const modeOf = (ctx: Context): string =>
    ctx.mode === "tui" || ctx.mode === "rpc" || ctx.mode === "json" || ctx.mode === "print"
      ? ctx.mode
      : ctx.hasUI === false
        ? "print"
        : "tui";

  /** The first frame on every door: who this copy is and what its conversation is doing now. */
  const hello = (ctx: Context): Frame => {
    const lead = led(ctx) ? leadOf(ctx) : undefined;
    const state = stateNow(ctx);
    stateSent = JSON.stringify(state);
    stateAt = Date.now();
    return {
      t: "hello",
      door: DOOR_VERSION,
      harness: HARNESS,
      pid: process.pid,
      session_id: ctx.sessionManager.getSessionId(),
      mode: modeOf(ctx),
      ...(lead ? { lead } : {}),
      state,
    };
  };

  /**
   * Hand the person's words to the harness, and say whether it took them: `steer` into
   * the turn running now, `queue` once it has ended; idle, either starts a turn.
   *
   * Neither harness says so through `sendUserMessage`: pi's returns nothing and reports a
   * refusal to its own error handlers, omp's settles only once a turn the words started
   * has run to its end. So the words are in only once the harness writes them into the
   * conversation as the person's message (`message_end`, [`taken`]); being queued, taken
   * off a queue (omp's `before_agent_start`) or passed through pi's `input` proves
   * nothing, since what lies after can still drop them. Kept by `id` until they go in
   * ([`says`]), so the turn they start is known as Steery's.
   *
   * Idle, both harnesses ask for a model and its credentials before they start a turn, and
   * drop the words where either is missing: pi out of sight, omp by throwing before it
   * takes them. So the door asks first, the way they ask ([`unstartable`]), and words they
   * would drop are refused `model-missing` with nothing handed over; omp throwing anyway
   * before it took them is refused the same.
   *
   * Handed and not written within [`HEARD_WITHIN_MS`] — steered words wait for every tool
   * call running to end — they are `no-answer`: on their way in, still tracked. A one-shot
   * run's words still waiting as the run ends never will go in, and are `ending`, so the
   * runtime puts them in afresh ([`refuseWaiting`]).
   *
   * One throw's words go into a conversation once. A say of a `thrown` throw whose words
   * this door already handed and that are still on their way in — handed and not yet
   * answered, or answered and not yet in the transcript — hands nothing: it is answered as
   * those words are, so a Retry that aims the throw again never puts in a second copy, and
   * its id is kept beside theirs ([`Say`]'s `also`), so a stop naming it stops the turn they
   * start. Words of the throw left behind while the conversation is idle with nothing
   * queued went nowhere — the harness let its queue go — and are handed again. The says
   * are carried to the next copy ([`Carried`]), so this holds across an update too.
   */
  const deliver = (
    ctx: Context,
    id: string,
    words: string,
    mode: "steer" | "queue",
    thrown: string | undefined,
    alone: boolean,
  ): Promise<Said> => {
    // Words of this throw already handed and not yet in: this say waits on them, and is
    // answered as they are, with nothing handed again.
    let handedBefore: string | undefined;
    if (thrown !== undefined) {
      // Still being handed: answered as those are. Only words refused with nothing handed
      // over are handed afresh; ones that went in, may yet, or never can are not.
      const handing = pending.find((one) => one.throw === thrown);
      if (handing) {
        return handing.said.then((said) =>
          "refused" in said && said.refused !== "no-answer" && said.refused !== "ending"
            ? deliver(ctx, id, words, mode, thrown, alone)
            : said,
        );
      }
      const waiting = says.find((one) => one.throw === thrown);
      if (waiting && idle(ctx) === true) says.splice(says.indexOf(waiting), 1);
      else if (waiting) {
        if (!waiting.also.includes(id) && waiting.id !== id) waiting.also.push(id);
        if (idsOf(waiting).some((one) => ours.has(one))) ours.add(id);
        handedBefore = waiting.words;
      }
    }
    const { promise, resolve } = Promise.withResolvers<Said>();
    // omp moves a session to another conversation in place: words are only ever handed
    // to, or heard in, the conversation they were asked for.
    const session = sessionId;
    const here = () => inSession(session);
    const one = {
      words: handedBefore ?? words.trim(),
      throw: thrown,
      handed: handedBefore !== undefined,
      said: promise,
      answer(said: Said) {
        const at = pending.indexOf(one);
        if (at < 0) return;
        pending.splice(at, 1);
        clearTimeout(timer);
        resolve(said);
      },
    };
    // Not handed in time: a turn being prepared never started, and nothing went in.
    const timer = setTimeout(
      () => one.answer(one.handed ? { refused: "no-answer" } : { refused: "harness-busy" }),
      HEARD_WITHIN_MS,
    );
    pending.push(one);
    if (handedBefore !== undefined) return promise;
    const send = async () => {
      if (!pending.includes(one)) return;
      if (!here()) {
        one.answer({ refused: "ending", error: "the session moved to another conversation" });
        return;
      }
      let running = !(ctx.isIdle?.() ?? true);
      const deliverAs = mode === "queue" ? "followUp" : "steer";
      // Idle, the harness starts a turn from the words only with a model and credentials
      // for it: asked first, the way it asks, so words it would drop are refused here with
      // nothing handed over.
      if (!running || alone) {
        const unready = await unstartable(ctx);
        if (unready) {
          one.answer({ refused: "model-missing", error: unready });
          return;
        }
        if (!pending.includes(one)) return;
        if (!here()) {
          one.answer({ refused: "ending", error: "the session moved to another conversation" });
          return;
        }
      }
      // Words that go in only while the conversation is idle are read so again here, as
      // they are handed over: the model check above awaited, and a turn, words of the
      // person's or their typing may have come in meanwhile. Nothing was handed. Read
      // idle now, they start a turn of their own.
      if (alone) {
        if (!quiet(ctx)) {
          one.answer({ refused: "harness-busy", error: "the conversation is working, has words waiting, or somebody is typing" });
          return;
        }
        running = false;
      }
      // Handed as the call begins: pi says it took them from inside the call, and a turn
      // they start may be under way before the call returns.
      one.handed = true;
      const say: Say = { id, also: [], words: one.words, mode, seen: false };
      if (thrown !== undefined) say.throw = thrown;
      says.push(say);
      if (says.length > SAYS_KEPT) says.shift();
      try {
        // pi reads `deliverAs` only while a turn runs, and refuses words without it then;
        // idle, the same call starts a turn. omp queues words given `deliverAs` even when
        // idle, where nothing takes them until some later turn: idle, they go in as the
        // person's own words would, which starts one.
        const sent =
          HARNESS === "pi" || running ? pi.sendUserMessage(words, { deliverAs }) : pi.sendUserMessage(words);
        if (sent instanceof Promise) {
          // omp's call settles once the turn its words started ends. Idle, it throws before
          // it takes them where it finds no model or credentials after all: nothing went in.
          sent.catch((error: unknown) => {
            if (!pending.includes(one)) return;
            if (running) {
              one.answer({ refused: "no-answer", error: String(error) });
              return;
            }
            if (says.includes(say)) says.splice(says.indexOf(say), 1);
            one.answer({ refused: "model-missing", error: String(error) });
          });
        }
      } catch (error) {
        one.handed = false;
        if (says.includes(say)) says.splice(says.indexOf(say), 1);
        one.answer({ refused: "refused", error: String(error) });
        return;
      }
    };
    void (async () => {
      while (starting || compacting) await (starting ?? compacting)?.promise;
      await send();
    })();
    return promise;
  };

  /**
   * Switch the conversation's model and thinking level for its next request, as the
   * person's own `/model` would, and say what is set now, read back from the harness. A
   * selector is looked up the way the harness's own `--model` reads it where it lends that
   * lookup to an extension (omp's `models`), else in its catalog by `<provider>/<id>`.
   *
   * A model the harness does not find or cannot set, or a level it holds the model below,
   * has it read its model files again ([`reread`]), once a pick, and the pick is applied
   * again: a model or a level enabled in those files since the harness started is taken
   * here, its turn left running. Still refused, the refusal's line says so and what its
   * person does: what only a new process of the harness reads — a provider extension
   * added since, a setting it reads once — is theirs to reload, never this file's.
   */
  const pick = async (ctx: Context, model: unknown, thinking: unknown): Promise<Said> => {
    const session = sessionId;
    if ((model !== undefined && typeof model !== "string") || (thinking !== undefined && typeof thinking !== "string")) {
      return { refused: "refused", error: "model and thinking are strings" };
    }
    let asked = false;
    let read = false;
    /** The model files read again, the first time the pick is refused: whether they were. */
    const again = async (): Promise<boolean> => {
      if (asked) return false;
      asked = true;
      read = await reread(ctx);
      return read;
    };
    /** The line of a pick still refused: why, and what its person does about it. */
    const still = (why: string): string => {
      const files = HARNESS === "pi" ? "models.json" : "models.yml";
      const remedy =
        HARNESS === "pi" ? "run /reload in it when it is idle, or restart it" : `restart omp (omp --resume ${session})`;
      return `${HARNESS} ${why}${read ? ` (${files} read again)` : ""}: ${remedy}`;
    };
    // The harness may change sessions while the pick waits on it. What it changed in the
    // one it left cannot be taken back: neither change the new session's settings nor
    // claim that the old conversation received the pick. Nothing changed, the old one is
    // free of this copy, as a conversation that ended here is.
    const moved = (changed: boolean): Said =>
      changed
        ? { refused: "no-answer", error: "the session moved while its pick was applied" }
        : { refused: "ending", error: "the session moved before its pick was applied" };
    const lookup = (spec: string): Model | undefined => {
      const slash = spec.indexOf("/");
      return (
        ctx.models?.resolve?.(spec) ??
        (slash > 0 ? ctx.modelRegistry?.find?.(spec.slice(0, slash), spec.slice(slash + 1)) : undefined)
      );
    };
    /** Whether the harness set it: pi says no, or throws, where it finds no credentials. */
    const set = async (found: Model): Promise<boolean> => {
      try {
        return (await pi.setModel?.(found)) === true;
      } catch {
        return false;
      }
    };
    if (model !== undefined) {
      if (!pi.setModel || (!ctx.models?.resolve && !ctx.modelRegistry?.find)) {
        return { refused: "selection-unsupported", error: `this ${HARNESS} cannot switch models` };
      }
      let found = lookup(model);
      if (!found && (await again())) {
        if (!inSession(session)) return moved(false);
        found = lookup(model);
      }
      if (!found) return { refused: "model-missing", error: still(`knows no model ${model}`) };
      let switched = await set(found);
      if (!switched && inSession(session) && (await again())) {
        if (!inSession(session)) return moved(false);
        switched = await set(lookup(model) ?? found);
      }
      if (!inSession(session)) return moved(switched);
      if (!switched) return { refused: "model-missing", error: still(`has no credentials for ${model}`) };
    }
    if (thinking !== undefined) {
      if (!pi.setThinkingLevel) {
        return { refused: "selection-unsupported", error: `this ${HARNESS} cannot set thinking` };
      }
      pi.setThinkingLevel(thinking);
      // Both harnesses hold a level to what the session's own copy of its model can do, a
      // copy taken from the catalog before the files were read again: the model is set
      // again from the catalog read now, then the level.
      if (current(ctx).thinking !== thinking && (await again())) {
        if (!inSession(session)) return moved(true);
        const held = current(ctx).model;
        const fresh = held === undefined ? undefined : lookup(held);
        if (fresh) await set(fresh);
        if (!inSession(session)) return moved(true);
        pi.setThinkingLevel(thinking);
      }
    }
    noteState();
    const now = current(ctx);
    if (thinking !== undefined && now.thinking !== thinking) {
      return {
        refused: "selection-unsupported",
        error: still(`holds ${now.model ?? "its model"} at ${now.thinking ?? "no"} thinking, not ${thinking}`),
      };
    }
    return { ok: now };
  };

  /**
   * A recap of this conversation by its own model, for another machine that carries it on
   * ([`RECAP_ASKED`]): one request through the harness's own provider, authenticated as
   * its turns are, on the history its next turn would be handed and under the system
   * prompt its turns run under, so a provider that caches it reads it from there. Nothing
   * is written into the conversation and no turn starts; a turn running goes on. pi
   * lends an extension the request itself (`modelRegistry.complete`); omp lends the key
   * (`modelRegistry.resolver`) and the loader hands over its `completeSimple`. Whatever
   * it answers is the runtime's to bound and redact.
   */
  const recap = async (ctx: Context): Promise<Said> => {
    const model = ctx.model;
    if (!model) return { refused: "model-missing", error: `${HARNESS} has no model selected` };
    const manager = ctx.sessionManager;
    const history = (manager.buildSessionProjection ?? manager.buildSessionContext)?.call(manager)?.messages;
    const convert = models?.convertToLlm;
    if (!Array.isArray(history) || history.length === 0 || !convert) {
      return { refused: "selection-unsupported", error: `this ${HARNESS} lends no history to read` };
    }
    const context = {
      systemPrompt: ctx.getSystemPrompt?.(),
      messages: [
        ...convert(history),
        { role: "user", content: [{ type: "text", text: RECAP_ASKED }], timestamp: Date.now() },
      ],
    };
    const signal = AbortSignal.timeout(RECAP_MS);
    const registry = ctx.modelRegistry;
    let answered: Answered | undefined;
    if (registry?.complete) {
      answered = await registry.complete(model, context, { signal });
    } else if (registry?.resolver && models?.completeSimple) {
      answered = await models.completeSimple(model, context, {
        apiKey: registry.resolver(model, manager.getSessionId()),
        sessionId: manager.getSessionId(),
        disableReasoning: true,
        signal,
      });
    } else {
      return { refused: "selection-unsupported", error: `this ${HARNESS} lends no model call` };
    }
    const text = (answered?.content ?? [])
      .filter((part) => part?.type === "text" && typeof part.text === "string")
      .map((part) => part.text)
      .join("")
      .trim();
    if (answered?.stopReason === "error" || answered?.stopReason === "aborted" || !text) {
      return { refused: "refused", error: answered?.errorMessage ?? "the model wrote no recap" };
    }
    return { ok: { text } };
  };

  /**
   * What the harness keeps running past a turn's stop, by name: an abort ends the turn and
   * leaves these going ([`Context.getAsyncJobSnapshot`]). None from a harness that has none.
   */
  const background = (ctx: Context): string[] => {
    const running = snapshot(ctx)?.running;
    return Array.isArray(running)
      ? running.map((job) => job?.label || job?.type || job?.id || "a job")
      : [];
  };

  /**
   * Abort a turn, and say so once no turn runs; with none running, nothing is stopped
   * (`stopped: false`). Either way it says what still runs past the turn (`running`), which
   * a stop that ends everything has not done. Named by a `say`, it is Steery's cancel of its
   * own words: only while the stretch running now is that say's ([`ours`]) and no words of
   * the person's wait to go into it. Any other turn is the person's, or holds their words,
   * and is refused `other-turn` with why. Named by no say, it is the person's own stop, said
   * from Steery (their Pause): whatever turn of this conversation runs stops, and is not
   * reminded to go on ([`parked`]) until the person's own words start one.
   */
  const stop = async (ctx: Context, say: unknown): Promise<Said> => {
    const session = sessionId;
    if (say !== undefined && (typeof say !== "string" || !say)) {
      return { refused: "refused", error: "a stop names the say whose turn it stops, or none for its person's" };
    }
    if (!ctx.abort) return { refused: "selection-unsupported", error: `this ${HARNESS} cannot abort` };
    const running = () => turning || ctx.isIdle?.() === false;
    // Its person's own stop holds the conversation parked whether or not a turn runs.
    if (say === undefined) parked = true;
    if (!running()) return { ok: { stopped: false, running: background(ctx) } };
    if (say !== undefined && !ours.has(say)) {
      const waiting = says.some((one) => idsOf(one).includes(say) && !one.seen);
      return {
        refused: "other-turn",
        error: waiting ? "its words wait to go into a turn of the person's" : "the turn running is not the one its words started",
      };
    }
    if (say !== undefined && ctx.hasPendingMessages?.() === true && !says.some((one) => !one.seen)) {
      return { refused: "other-turn", error: "the person's own words wait to go into it" };
    }
    ctx.abort();
    const deadline = Date.now() + STOPPED_WITHIN_MS;
    while (inSession(session) && running() && Date.now() < deadline) {
      const { promise, resolve } = Promise.withResolvers<void>();
      setTimeout(resolve, 50);
      await promise;
    }
    if (!inSession(session)) return { refused: "no-answer", error: "the session moved while its turn was stopping" };
    noteState();
    return running()
      ? { refused: "no-answer", error: "a turn still runs" }
      : { ok: { stopped: true, running: background(ctx) } };
  };

  /**
   * Forget everything before a cut in favour of Steery's `summary`, through the harness's own
   * compaction with no model run: its `session_before_compact` hook answers this ask alone,
   * known by its marker ([`ownCompaction`]). Only while the conversation is quiet: either
   * harness aborts a turn it compacts under. Where the cut ([`cutOf`]) would forget fewer
   * than `least` tokens, the provider still caches the context and the cut is too small to
   * pay for writing what stays to its cache again ([`worth`]), or the harness finds nothing
   * to compact, nothing is done (`forgot: 0`, `from` where the context starts now). Words
   * asked meanwhile wait ([`compacting`]). Answered `{from, forgot}`, `from` when the cut
   * entry was written, once the harness wrote Steery's compaction; whatever else became of
   * it within [`COMPACTED_WITHIN_MS`] is refused with the harness's own line.
   */
  const compact = async (ctx: Context, id: string, before: unknown, summary: unknown, least: unknown): Promise<Said> => {
    if (typeof before !== "number" || !Number.isSafeInteger(before) || before < 0) {
      return { refused: "refused", error: "before is milliseconds since 1970" };
    }
    if (typeof least !== "number" || !Number.isSafeInteger(least) || least < 0) {
      return { refused: "refused", error: "least is a count of tokens" };
    }
    if (typeof summary !== "string" || !summary.trim()) return { refused: "refused", error: "no summary" };
    if (!ctx.compact) return { refused: "refused", error: `this ${HARNESS} cannot compact` };
    // A turn being prepared, or between the runs it goes on with, reads as idle: compacting
    // then would abort it.
    if (turning || !quiet(ctx)) {
      return { refused: "harness-busy", error: "the conversation is working, has words waiting, or somebody is typing" };
    }
    const branch = entriesOf(ctx.sessionManager.getBranch?.());
    const rate = tokenRate(branch);
    const { from, cut, forgot } = cutOf(branch, before, rate);
    const at = writtenAt(cut);
    if (
      !cut ||
      typeof cut.id !== "string" ||
      at === undefined ||
      forgot < least ||
      !worth(branch, forgot, Math.ceil(summary.length * rate), Date.now())
    ) {
      return { ok: { from, forgot: 0 } };
    }
    const marker = `${COMPACT_MARK}${id}`;
    const own = { id, marker, summary, cut: cut.id, seen: false };
    ownCompaction = own;
    compacting ??= Promise.withResolvers<void>();
    noteState();
    const { promise: done, resolve, reject } = Promise.withResolvers<void>();
    try {
      // omp's call settles once it compacted; pi's returns at once and says so after.
      if (HARNESS === "pi") ctx.compact({ customInstructions: marker, onComplete: () => resolve(), onError: reject });
      else void Promise.resolve(ctx.compact(marker)).then(() => resolve(), reject);
    } catch (error) {
      reject(error);
    }
    const late = Promise.withResolvers<never>();
    const timer = setTimeout(
      () => late.reject(new Error(`${HARNESS} did not compact within ${COMPACTED_WITHIN_MS / 1000} s`)),
      COMPACTED_WITHIN_MS,
    );
    const failed = await Promise.race([done, late.promise]).then(
      () => undefined,
      (error: unknown) => (error instanceof Error ? error.message : String(error)),
    );
    clearTimeout(timer);
    if (ownCompaction === own) ownCompaction = undefined;
    // pi's own `session_compact` and `session_compact_failed` end what it compacts.
    if (HARNESS === "omp") compacted();
    noteState();
    if (own.seen) return { ok: { from: at, forgot } };
    if (failed === undefined) return { refused: "refused", error: `${HARNESS} compacted without Steery's summary` };
    if (failed.startsWith("Nothing to compact") || failed === "Already compacted") return { ok: { from, forgot: 0 } };
    return { refused: "refused", error: failed };
  };

  /**
   * What one ask of the runtime's asks, answered. A sub-agent's copy — whose conversation
   * is its lead's — takes no words, no pick, no stop and no compaction. A conversation's own copy takes
   * words and picks where a person is at it (`tui`, `rpc`) or a turn runs: a one-shot run's
   * words before its own turn would start a turn of their own ahead of the run's words.
   */
  const answer = (request: Frame): Promise<Said> | Said => {
    const ctx = live;
    const op = request.op;
    if (ending || !wanted || !ctx) return { refused: "ending" };
    if (led(ctx) && op !== "recap") return { refused: "refused", error: "a sub-agent's conversation takes this at its lead" };
    if ((op === "say" || op === "pick") && oneShot(ctx) && !turning) {
      return { refused: "refused", error: "this one-shot run takes nothing before its own turn" };
    }
    switch (op) {
      case "say": {
        const { words, mode } = request;
        if (typeof words !== "string" || !words.trim()) return { refused: "refused", error: "no words" };
        if (mode !== "steer" && mode !== "queue") return { refused: "refused", error: "mode is steer or queue" };
        if (typeof request.idle !== "boolean") return { refused: "refused", error: "a say says idle" };
        // The throw whose words they are, where an aim's: one throw's go in once ([`deliver`]).
        const thrown = request.throw === undefined ? undefined : typeof request.throw === "string" ? request.throw : "";
        if (thrown === "") return { refused: "refused", error: "a say's throw is the throw's id" };
        // Words said `idle` — Steery's own nobody asked for now, another member's, a
        // reviewed answer — go in only while nothing else would: no turn, nothing queued,
        // nobody typing at the input line, read as they arrive and again as they are
        // handed over ([`deliver`]).
        if (request.idle && !quiet(ctx)) {
          return { refused: "harness-busy", error: "the conversation is working, has words waiting, or somebody is typing" };
        }
        return deliver(ctx, String(request.id), words, mode, thrown, request.idle);
      }
      case "pick":
        return pick(ctx, request.model, request.thinking);
      case "stop":
        return stop(ctx, request.say);
      case "recap":
        return recap(ctx);
      case "compact":
        return compact(ctx, String(request.id), request.before, request.summary, request.least);
      default:
        return { refused: "refused", error: `no op ${String(op)}` };
    }
  };

  /**
   * Asks by id: one sent again is answered from the first, and what it did is never done
   * twice, for the last [`REMEMBERED`]. One refused other than `no-answer` did nothing —
   * refused before anything reached the harness — and is forgotten once answered, so the
   * same id asked again later acts afresh. A say answered `no-answer` is answered `ok` once
   * its words are written ([`message_start`]); asked again while they are still unwritten
   * and nothing holds them — no turn, nothing queued — they went nowhere, and are handed afresh.
   */
  const addressed = (id: string, request: Frame): Promise<Said> => {
    const ctx = live;
    const session = sessionId;
    if (!ctx || session === undefined || !inSession(session)) return Promise.resolve({ refused: "ending" });
    const answered = answeredIn(session);
    const ask = (): Promise<Said> => {
      const asked = Promise.resolve()
        .then((): Promise<Said> | Said => (inSession(session) ? answer(request) : { refused: "ending" }))
        .catch((error: unknown): Said => ({ refused: "refused", error: String(error) }));
      answered.set(id, asked);
      if (answered.size > REMEMBERED) answered.delete(answered.keys().next().value as string);
      void asked.then((one) => {
        if ("refused" in one && one.refused !== "no-answer" && answered.get(id) === asked) answered.delete(id);
      });
      return asked;
    };
    const first = answered.get(id);
    if (!first) return ask();
    return first.then((one) => {
      const now = answered.get(id);
      if (now !== undefined && now !== first) return now;
      if (!("refused" in one) || one.refused !== "no-answer" || now === undefined) return one;
      const unwritten = says.filter((one) => idsOf(one).includes(id));
      if (unwritten.length === 0 || !live || !inSession(session) || idle(live) !== true || compacting) return one;
      for (const gone of unwritten) says.splice(says.indexOf(gone), 1);
      return ask();
    });
  };

  /**
   * Answer the words handed over and not yet seen taken. They may still be in — the
   * harness's queue is its own — so they are never said to be refused: said `no-answer`,
   * they are neither put in again nor counted lost. At the end of a one-shot run (`ran`)
   * they cannot be: its process ends with its queue, so they are `ending`, nothing went in,
   * and the runtime puts them in afresh ([`deliver`]).
   */
  const refuseWaiting = (ran = false) => {
    for (const one of [...pending]) one.answer({ refused: one.handed && !ran ? "no-answer" : "ending" });
  };

  /** Stop taking words, answer the ones waiting, and let those answers go out on the door. */
  const release = async () => {
    ending = true;
    begun();
    compacted();
    refuseWaiting();
    const { promise, resolve } = Promise.withResolvers<void>();
    setTimeout(resolve, 0);
    await promise;
  };

  /** Close the door for good. */
  const shut = () => {
    wanted = false;
    clearTimeout(redial);
    redial = undefined;
    cut();
  };

  /**
   * Whether the conversation open here is another's sub-agent: omp says it runs one, its
   * process inherited a lead naming another conversation, another session in this process
   * leads it, or omp filed it inside another's transcript. A sub-agent's door takes no
   * words — its lead's does — and it is never told to ask its person: its lead does both.
   */
  const led = (ctx: Context): boolean =>
    ctx.agent?.kind === "sub" ||
    (INHERITED !== undefined && INHERITED !== marker(ctx)) ||
    (leading !== undefined && leading !== self) ||
    speaker(ctx).agent_id !== undefined;

  /**
   * The session is here: it leads this process where nothing else does, carries Steery's
   * MCP entry in pi, and opens its door. A sub-agent's door is filed under its lead; one
   * whose lead nothing names opens none.
   */
  const begin = (ctx: Context) => {
    unheld = false;
    live = ctx;
    sessionId = ctx.sessionManager.getSessionId();
    hold(ctx);
    // The first session here that no other leads is the lead of every process it starts,
    // said before any of them is started.
    if (!led(ctx)) {
      leading = self;
      process.env[LEAD] = marker(ctx);
    }
    if (HARNESS === "pi") signals();
    register(ctx);
    if (wanted || (led(ctx) && !leadOf(ctx))) return;
    wanted = true;
    dial();
  };
  /**
   * Answer a `compact` ask a turn the runtime started carries in a file ([`COMPACT_ASK`]),
   * as the door answers one ([`compact`]), before the turn's prompt runs: both harnesses
   * await `session_start` before it, and their prompt waits for the compaction to be over.
   * The answer frame goes to `compacted.json` beside the ask, readable by its owner only;
   * an ask that cannot be read is answered nothing. The ask is taken from its file before it
   * is read: omp's shells copy the process's own environment, which deleting the variable
   * from `process.env` leaves as it was, so a harness a tool starts finds it too.
   */
  const compactAsked = async (ctx: Context, file: string) => {
    const taken = `${file}.taken`;
    let ask: unknown;
    try {
      renameSync(file, taken);
      ask = JSON.parse(readFileSync(taken, "utf8"));
    } catch {
      return;
    } finally {
      rmSync(taken, { force: true });
    }
    if (!ask || typeof ask !== "object" || !("t" in ask) || ask.t !== "ask" || !("op" in ask) || ask.op !== "compact") {
      return;
    }
    const id = "id" in ask ? ask.id : undefined;
    if (typeof id !== "string" || !id) return;
    const before = "before" in ask ? ask.before : undefined;
    const summary = "summary" in ask ? ask.summary : undefined;
    const least = "least" in ask ? ask.least : undefined;
    const said: Said = led(ctx)
      ? { refused: "refused", error: "a sub-agent's conversation takes this at its lead" }
      : await compact(ctx, id, before, summary, least).catch(
          (error: unknown): Said => ({ refused: "refused", error: String(error) }),
        );
    try {
      writeFileSync(join(dirname(file), "compacted.json"), JSON.stringify({ t: "answer", id, ...said }), { mode: 0o600 });
    } catch {
      // The runtime reads no answer, and says the words as it would without one.
    }
  };
  /**
   * pi: what a kept process the runtime started on a pick of its own refuses, where pi does
   * not run exactly that pick (`<word>: why`): pi takes a model it can only partly match
   * with a warning, and clamps a thinking level it cannot use, where Steery promised its
   * person the one they chose. Read at the process's first start, before its door opens
   * and any words go in; a `pick` through the door later moves it on its own terms.
   */
  const unpicked = (ctx: Context): string | undefined => {
    const model = pi.getFlag?.("steery-model");
    const thinking = pi.getFlag?.("steery-thinking");
    const resolved = ctx.model ? `${ctx.model.provider}/${ctx.model.id}` : "no model";
    if (typeof model === "string" && resolved !== model && !(!model.includes("/") && ctx.model?.id === model)) {
      return `model-missing: ${model} resolved as ${resolved}`;
    }
    const level = pi.getThinkingLevel?.();
    if (typeof thinking === "string" && level !== thinking) {
      return `selection-unsupported: thinking ${thinking} resolved as ${level ?? "none"}`;
    }
    return undefined;
  };
  pi.on("session_start", (event, ctx) => {
    // A pick not met is refused in one line the runtime reads off standard error, and the
    // process ends before anything went in: its turn ends on that refusal.
    const refused = HARNESS === "pi" && event.reason === "startup" ? unpicked(ctx) : undefined;
    if (refused) {
      process.stderr.write(`steery-refused: ${refused}\n`);
      process.exit(1);
    }
    const id = ctx.sessionManager.getSessionId();
    const stood = HARNESS === "pi" && event.reason === "reload" ? reloading().get(id) : undefined;
    reloading().delete(id);
    if (stood) {
      ({ asked, parked, waiting, reminded, turning, began, retrying, continued, dialog } = stood);
      says.splice(0, says.length, ...stood.says);
      ours = new Set(stood.ours);
      calls.clear();
      for (const call of stood.calls) calls.set(call.id, call);
    }
    begin(ctx);
    moved(event, ctx);
    moment("SessionStart", document(ctx));
    // Only the process's first start finds the ask: nothing it starts may inherit it.
    const file = process.env[COMPACT_ASK];
    delete process.env[COMPACT_ASK];
    return file ? compactAsked(ctx, file) : undefined;
  });
  // omp moves one session to another in place, without a new start: the door closes and
  // opens again, now for the conversation the session is. Its person's `/new` leaves the
  // conversation it was in for good, as pi's does with a `session_shutdown` of its own; a
  // resume or a fork leaves it to be come back to. pi replaces the whole extension
  // runtime instead — `session_shutdown` here, then `session_start` in a copy built anew —
  // and has neither event.
  const moved = (event: Event, ctx: Context) => {
    live = ctx;
    sessionId = ctx.sessionManager.getSessionId();
    hold(ctx);
    if (leading === self) process.env[LEAD] = marker(ctx);
    const gone = typeof event.previousSessionFile === "string" ? basename(event.previousSessionFile, ".jsonl") : "";
    const at = gone.lastIndexOf("_");
    if (HARNESS === "omp" && event.reason === "new" && at >= 0) {
      moment("SessionEnd", {
        session_id: gone.slice(at + 1),
        cwd: ctx.cwd,
        mode: ctx.mode,
        reason: "new",
        at: Date.now(),
      });
    }
  };
  if (HARNESS === "omp") {
    // Words asked for the conversation the session leaves never go into the one it moves
    // to: those not yet handed over are refused `ending`, those handed and not seen taken
    // `no-answer`, and Steery's words of that conversation are forgotten. The door opens
    // again for the conversation the session is now.
    const switched = async (event: Event, ctx: Context) => {
      const was = sessionId;
      moved(event, ctx);
      if (was === undefined || was === ctx.sessionManager.getSessionId()) return;
      // Until the door opens again, asks the old door carries are refused `ending`; the
      // refusals go out on it before it closes.
      ending = true;
      refuseWaiting();
      says.splice(0, says.length);
      ours = new Set();
      turnSays = new Set();
      settled.splice(0, settled.length);
      continued = undefined;
      const { promise, resolve } = Promise.withResolvers<void>();
      setTimeout(resolve, 0);
      await promise;
      if (!wanted) return;
      ending = false;
      cut();
      backoff = REDIAL_MIN_MS;
      dial();
    };
    pi.on("session_switch", switched);
    pi.on("session_branch", switched);
  }
  // The person has just sent something: the moment a paragraph reaches the agent. Both
  // harnesses' own notices to their agent arrive as `<system-reminder>` blocks, which the
  // agent reads as the harness speaking; bare text beside the person's turn is read,
  // rightly, as words the person did not write, and a line at the end of the system
  // text is read as nothing much at all.
  pi.on("before_agent_start", async (event, ctx) => {
    live = ctx;
    // omp taking a batch off its queue is no proof the words are in: only `message_end` is.
    // Whose the stretch is now. omp going on with the reminder its stop hook asked for
    // carries on whoever's turn that was.
    const input = typeof event.prompt === "string" ? event.prompt.trim() : "";
    if (!turning) {
      opening = true;
      began = Date.now();
      turnSays = new Set();
      failure = undefined;
      stopped = undefined;
      keptGoing = false;
    }
    began ??= Date.now();
    const going = continued !== undefined && input === continued.trim();
    const said = whose(input);
    if (going) opening = false;
    else heard(said);
    continued = undefined;
    if (!starting && (ctx.isIdle?.() ?? false)) starting = Promise.withResolvers<void>();
    turning = true;
    retrying = false;
    noteState();
    asked = false;
    waiting = false;
    // A park holds until the person's own words start a turn: Steery's words, a reminder
    // going on, or a background job waking the conversation leave it parked.
    if (!going && said.theirs) parked = false;
    reminded = false;
    const prompt = typeof event.prompt === "string" ? { prompt: event.prompt } : {};
    const line = sayOf(await awaited(moment("UserPromptSubmit", document(ctx, prompt)), PROMPT_MS));
    if (!line) return undefined;
    return {
      message: {
        customType: "steery",
        content: `<system-reminder reason="steery">\n${line}\n</system-reminder>`,
        display: false,
      },
    };
  });
  pi.on("agent_start", (_event, ctx) => {
    live = ctx;
    begun();
    turning = true;
    began ??= Date.now();
    noteState();
  });
  // omp's own retries between runs: it goes on by itself, so the conversation is not idle.
  pi.on("auto_retry_start", (_event, ctx) => {
    live = ctx;
    retrying = true;
    noteState();
  });
  pi.on("auto_retry_end", (_event, ctx) => {
    live = ctx;
    retrying = false;
    noteState();
  });
  // Words the harness took, as they go into the transcript: its queue just changed. A
  // person's message is input; Steery's, once its batch was read as the turn took it, is
  // read again here and nothing more.
  pi.on("message_start", (event, ctx) => {
    live = ctx;
    const text = spoken(event.message);
    if (text !== undefined) {
      const one = says.find((one) => one.words === text.trim());
      if (one) says.splice(says.indexOf(one), 1);
      writing = one;
      if (!one?.seen) heard({ mine: one ? [one] : [], theirs: !one });
    }
    noteState();
  });
  // Both harnesses write the person's message into the transcript as its `message_end`
  // handlers return: words not heard sooner ([`deliver`]) are heard here. An answer of the
  // model's says whether the turn stands on an error: one that stopped on an error until
  // a later one gets past it, a stopped request saying nothing either way; and how the
  // turn's last answer stopped, as the turn's `Stop` says it.
  pi.on("message_end", (event, ctx) => {
    live = ctx;
    taken(spoken(event.message));
    causeWritten = undefined;
    const answer = event.message as { role?: unknown; stopReason?: unknown; errorMessage?: unknown } | null | undefined;
    if (answer && typeof answer === "object" && answer.role === "assistant") {
      if (answer.stopReason === "error") failure = typeof answer.errorMessage === "string" ? answer.errorMessage : "";
      else if (answer.stopReason !== "aborted") failure = undefined;
      stopped = {
        ...(typeof answer.stopReason === "string" ? { stop_reason: answer.stopReason } : {}),
        ...(typeof answer.errorMessage === "string" ? { error_message: answer.errorMessage } : {}),
      };
    }
    noteState();
    return undefined;
  });
  // Every other moment the state's fields may change at says them again.
  const changed = (_event: Event, ctx: Context) => {
    live = ctx;
    noteState();
  };
  pi.on("turn_start", changed);
  pi.on("turn_end", changed);
  // A move along the conversation's tree: the same conversation, from another entry.
  pi.on("session_tree", changed);
  // The calls the agent is inside, and the calls made from inside them that still run: a
  // codemode script's calls among them, which its transcript never names as calls of
  // their own. Ids of calls made from inside another are `<parent id>/<n>`, nested as
  // deep as the calls are, and each is filed under the call the agent made.
  const outer = (id: string): Call | undefined => calls.get(id) ?? calls.get(id.split("/")[0] ?? "");
  pi.on("tool_execution_start", (event, ctx) => {
    live = ctx;
    const id = str(event.toolCallId);
    if (!id) return;
    // Named as a Call names it (PROTOCOL.md §4.1): Steery's own as `mcp__steery__<tool>`.
    const own = steerys(event.toolName);
    const tool = own === undefined ? str(event.toolName) : `mcp__steery__${own}`;
    const parent = str(event.parentToolCallId);
    if (!parent) calls.set(id, { id, tool, since: Date.now(), children: [] });
    else outer(parent)?.children.push({ id, tool });
    noteState();
  });
  pi.on("tool_execution_end", (event, ctx) => {
    live = ctx;
    const id = str(event.toolCallId);
    const parent = str(event.parentToolCallId);
    if (!parent) {
      if (calls.delete(id)) {
        ended.push({ id, at: Date.now() });
        ended.splice(0, ended.length - ENDED_KEPT);
      }
    } else {
      const call = outer(parent);
      if (call) call.children = call.children.filter((child) => child.id !== id);
    }
    noteState();
  });
  // A dialog of the harness's own waits on its person: a local dialog, never a request.
  pi.on("ui_prompt_start", (event, ctx) => {
    live = ctx;
    const kind = DIALOG_KINDS.find((one) => one === event.kind) ?? "custom";
    dialog = { kind, title: str(event.title), since: Date.now() };
    noteState();
  });
  pi.on("ui_prompt_end", (_event, ctx) => {
    live = ctx;
    dialog = null;
    noteState();
  });
  if (HARNESS === "pi") {
    // pi says when the person switches model or thinking; omp has no such event, and its
    // next turn or the door's own look says it.
    pi.on("model_select", changed);
    pi.on("thinking_level_select", changed);
    pi.on("session_compact_failed", (event, ctx) => {
      live = ctx;
      compacted();
      // pi's own overflow recovery could not make the context fit — the model refused the
      // prompt as over its limit and compacting it failed, or failed again — so the run
      // ends on that refusal: what it settles on is a context that is full.
      if (event.reason === "overflow" && event.aborted !== true) overflowed = true;
      noteState();
    });
  }
  /**
   * Who made the compaction `session_before_compact` announces: pi says why itself
   * (`manual`, `threshold`, `overflow`; pi 1.1.0), omp only through its
   * `auto_compaction_start` ([`autoCompaction`]). pi before 1.1.0 says nothing: nothing is
   * written, and the reader says the harness made it, why unrecorded.
   */
  const compactor = (reason: unknown): { by: "harness"; reason?: string } | { by: "person" } | undefined => {
    if (HARNESS === "omp") return autoCompaction ? { by: "harness", ...autoCompaction } : { by: "person" };
    if (reason === "manual") return { by: "person" };
    return typeof reason === "string" ? { by: "harness", reason } : undefined;
  };
  if (HARNESS === "omp") {
    pi.on("auto_compaction_start", (event) => {
      autoCompaction = typeof event.reason === "string" ? { reason: event.reason } : {};
    });
    pi.on("auto_compaction_end", () => {
      autoCompaction = undefined;
    });
  }
  // pi compacts after a run as well as when its person asks, and takes no words until it is
  // done ([`compacting`]). Steery's own compaction is answered here with its summary and
  // cut, and no model runs ([`compact`]); every other compaction passes untouched, after
  // saying who made it ([`COMPACTION`]). One carrying Steery's marker for an ask that already
  // gave up is cancelled: the harness would summarise it with its model.
  pi.on("session_before_compact", (event, ctx) => {
    live = ctx;
    if (HARNESS === "pi") compacting ??= Promise.withResolvers<void>();
    noteState();
    const asked = event.customInstructions;
    if (typeof asked !== "string" || !asked.startsWith(COMPACT_MARK)) {
      const by = compactor(event.reason);
      const cause = by && JSON.stringify(by);
      if (cause && cause !== causeWritten) {
        try {
          pi.appendEntry?.(COMPACTION, by);
          causeWritten = cause;
        } catch {
          // A transcript that cannot be written says only the compaction.
        }
      }
      return undefined;
    }
    const own = ownCompaction;
    if (own?.marker !== asked) return { cancel: true };
    const preparation = event.preparation && typeof event.preparation === "object" ? event.preparation : {};
    const said = "previousSummary" in preparation ? preparation.previousSummary : undefined;
    const latest = entriesOf(event.branchEntries).findLast((entry) => entry.type === "compaction")?.summary;
    const previous = typeof said === "string" ? said : typeof latest === "string" ? latest : "";
    const tokensBefore = "tokensBefore" in preparation ? preparation.tokensBefore : undefined;
    return {
      compaction: {
        summary: previous ? `${previous}\n\n${own.summary}` : own.summary,
        firstKeptEntryId: own.cut,
        tokensBefore: typeof tokensBefore === "number" ? tokensBefore : 0,
        details: { steery: own.id },
      },
    };
  });
  pi.on("session_compact", (event, ctx) => {
    live = ctx;
    causeWritten = undefined;
    const details = entriesOf([event.compactionEntry])[0]?.details;
    const own = ownCompaction;
    if (own && details && typeof details === "object" && "steery" in details && details.steery === own.id) {
      own.seen = true;
    }
    if (HARNESS === "pi") compacted();
    noteState();
  });
  // Every call of a compacted context, whoever compacted it, opens before its summary
  // ([`opened`]), and no call spends a cache breakpoint on its tools ([`unanchored`]).
  pi.on("context", (event, ctx) => {
    const messages = opened(event.messages, ctx.model);
    return messages && { messages };
  });
  pi.on("before_provider_request", (event) => unanchored(event.payload));
  /**
   * Steery's words to keep a turn about to end going, from the runtime (`settle`'s
   * answer, `ok.say`): none where the turn asked its person, its agent parked the
   * conversation, it waits on a Steery request, it was already kept going once
   * (`continued`), or the door is not open. The runtime holds the rule and the words; this
   * copy says what it saw.
   */
  const settle = async (continued: boolean): Promise<string | undefined> => {
    if (!welcomed) return undefined;
    const id = nextId();
    const said = awaited(id, SETTLE_MS);
    settling.add(id);
    if (!send({ t: "settle", id, asked, parked, waiting, continued })) moments().waiting.get(id)?.(undefined);
    const answered = await said;
    settling.delete(id);
    return sayOf(answered);
  };
  // The turn is about to end. A turn that asked its person nothing is reminded once, and
  // the agent decides. A sub-agent is never reminded: its lead asks.
  if (HARNESS === "omp") {
    // omp's own stop hook, which it offers the main session and never a sub-agent. Once
    // is omp's to count: it runs a reminder as a turn of its own, which prepares like any
    // turn the person starts, so only `stop_hook_active` still knows the turn was already
    // reminded. The turn it runs carries on the one that ended ([`continued`]). omp offers
    // it after a run whose last answer failed too (omp 18.4.12 `AgentSession`); as in pi,
    // only a run whose last answer stopped on its own is reminded.
    pi.on("session_stop", async (event, ctx) => {
      live = ctx;
      const last = event.last_assistant_message;
      const how = last && typeof last === "object" && "stopReason" in last ? last.stopReason : undefined;
      if (how === "error" || how === "aborted" || led(ctx)) return undefined;
      const reminder = await settle(event.stop_hook_active === true);
      if (!reminder) return undefined;
      keptGoing = true;
      continued = reminder;
      return { continue: true, additionalContext: reminder };
    });
  } else {
    // pi's own last word before a run settles: a handler may add entries and ask for one
    // more request, which pi makes inside the same run — no new turn the person started,
    // so `reminded` holds until they start one. Only a run whose last answer stopped on
    // its own is reminded: one that failed was retried by pi or is reported as failed,
    // and one the person stopped is theirs.
    pi.on("agent_before_settle", async (event, ctx) => {
      live = ctx;
      if (event.outcome !== "completed" || reminded || led(ctx)) return undefined;
      const reminder = await settle(false);
      if (!reminder) return undefined;
      keptGoing = true;
      reminded = true;
      return {
        entries: [{ type: "custom_message", customType: "steery", content: reminder, display: false }],
        continue: true,
      };
    });
  }
  // A call the agent makes: a shell command that raises or reads a request through the
  // `steery` command line is asking, whoever runs it — the agent, or a script the agent
  // wrote. Only the agent's own calls are moments: a call made from inside another is said
  // by the door's `calls`. The moment carries the call as the harness gave it — its tool,
  // its id and its input — and the driver's `hook/read` reads what call it is.
  pi.on("tool_call", async (event, ctx) => {
    live = ctx;
    const { tool } = called(event);
    const op = ran(commandOf(event.input))?.op;
    if (op === "requests.create" || op === "requests.get") asked = true;
    if (typeof event.parentToolCallId === "string" && event.parentToolCallId) return undefined;
    const id = moment(
      "PreToolUse",
      document(ctx, { tool_name: event.toolName, tool_use_id: event.toolCallId, tool_input: event.input ?? null }),
    );
    // A call to Steery's own tools, where the MCP connection names no conversation, goes
    // once the runtime wrote down whose it is: the runtime answers this moment only then.
    if (tool !== undefined && !REGISTERS) {
      await awaited(id, STAMP_MS);
      return undefined;
    }
    // A shell command a resource's cue names is told, with the call's result, which
    // resource it acts on and who holds it: omp hands a `tool_call` handler's
    // `additionalContext` to the model beside the result. Only a shell command waits, and
    // never longer than the bound; anything else goes on at once.
    if (HARNESS !== "omp" || !commandOf(event.input)) return undefined;
    const line = sayOf(await awaited(id, CALL_MS));
    if (!line) return undefined;
    return { additionalContext: `<system-reminder reason="steery">\n${line}\n</system-reminder>` };
  });
  pi.on("tool_result", async (event, ctx) => {
    live = ctx;
    const { tool, args } = called(event);
    // Each Steery call says whether the turn now waits ([`waits`]); the last one stands.
    const command = ran(commandOf(event.input));
    if (tool || command) waiting = event.isError !== true && !!command && waits(command, event.content);
    // `sessions_describe` with a `park` parks the conversation, once Steery took it: a
    // refused one leaves the conversation live.
    if (
      event.isError !== true &&
      tool === "sessions_describe" &&
      !!args &&
      typeof args === "object" &&
      "park" in args &&
      !!args.park &&
      typeof args.park === "object"
    ) {
      parked = true;
    }
    if (typeof event.parentToolCallId === "string" && event.parentToolCallId) return undefined;
    // The call that came back, by the id the transcript writes it under, and whether it failed.
    const id = moment(
      "PostToolUse",
      document(ctx, { tool_name: event.toolName, tool_use_id: event.toolCallId, is_error: event.isError === true }),
    );
    // A sub-agent works as one long turn: omp hands a `tool_result` handler every call of
    // every sub-agent session and delivers its `additionalContext` outside the tool's
    // output (omp 18.4.4), so a led copy hears there, now and then, that its lines are owed
    // (`Harness::tells_working`). The runtime decides whether there is anything to say; a
    // lead's own copy is told at its person's next instruction instead.
    if (HARNESS !== "omp" || !led(ctx)) return undefined;
    const line = sayOf(await awaited(id, CALL_MS));
    if (!line) return undefined;
    return { additionalContext: `<system-reminder reason="steery">\n${line}\n</system-reminder>` };
  });
  // The turn is over. pi's `agent_end` can be followed by a retry, a compaction or queued
  // words, and `agent_settled` is when none is left. omp's run is over at an `agent_end`
  // that does not go on by itself (`willContinue`: a retry, a continuation, its stop hook's
  // reminder), or that yields while work it started runs on in the background or waits to
  // hand its results back, which wakes the conversation later (omp 18.4.12
  // `awaitingAsyncWork`, which an extension is not told apart from a run going on). One
  // that goes on is still the turn. A one-shot run takes no words past its own turn: what
  // waits is answered.
  pi.on(HARNESS === "pi" ? "agent_settled" : "agent_end", (event, ctx) => {
    live = ctx;
    // pi keeps no classification beside the error it recorded: the run's end says, in the
    // conversation's own transcript, that it ended with its context full, for the
    // machine's witness to read beside that error (`witness/failure.rs`, `PI_CONTEXT_FULL`).
    if (overflowed) {
      overflowed = false;
      try {
        pi.appendEntry?.(CONTEXT_FULL, {});
      } catch {
        // A transcript that cannot be written says only the error it holds.
      }
    }
    if (event.willContinue === true) {
      if (jobs(ctx) === 0) {
        noteState();
        return;
      }
    }
    begun();
    compacted();
    // A turn Steery's words began says how it ended, for the run its say's aim reports.
    if (began !== undefined && turnSays.size > 0) {
      settled.push({
        began_at: began,
        at: Date.now(),
        says: [...turnSays],
        failed: failure !== undefined,
        ...(failure ? { error: failure } : {}),
      });
      settled.splice(0, settled.length - SETTLED_KEPT);
    }
    const how = stopped ?? {};
    turnSays = new Set();
    failure = undefined;
    stopped = undefined;
    const carried = keptGoing;
    keptGoing = false;
    turning = false;
    began = undefined;
    calls.clear();
    if (oneShot(ctx)) refuseWaiting(true);
    noteState();
    moment("Stop", document(ctx, { ...how, continued: carried }));
  });
  /** Let go of all this session holds here: its door and the lead. */
  const letGo = async () => {
    await release();
    shut();
    unlead();
  };
  /** A session that led this process leads it no more: the next here may. */
  const unlead = () => {
    if (leading !== self) return;
    leading = undefined;
    if (INHERITED === undefined) delete process.env[LEAD];
    else process.env[LEAD] = INHERITED;
  };
  const running: Running = {
    /**
     * The loader moves this session onto a newer copy, or pi reloads: this one lets go of
     * the door and the lead, and says where the session stands. Words handed over and not
     * yet seen taken are said `no-answer`; moments the door did not carry wait for the
     * next copy's.
     */
    async stop() {
      unheld = true;
      unhold();
      await letGo();
      return {
        asked,
        parked,
        waiting,
        reminded,
        turning,
        began,
        retrying,
        continued,
        says: [...says],
        ours: [...ours],
        turnSays: [...turnSays],
        failure,
        settled: [...settled],
        calls: [...calls.values()],
        ended: [...ended],
        dialog,
      };
    },
  };
  // Both harnesses wait for this handler before tearing the session down, and pi's RPC
  // mode ends the process straight after: the conversation's end is said before it goes,
  // with how it ends, so the runtime can tell its person closing it from a signal or a run
  // nobody sat at. Its moments go out within a short bound; what does not stays on the
  // process for the next copy's door.
  pi.on("session_shutdown", async (event, ctx) => {
    // pi's `/reload` builds this copy again over the same conversation, which goes on:
    // nothing ended, and the copy built next takes up where it stood and says it started.
    if (HARNESS === "pi" && event.reason === "reload") {
      reloading().set(ctx.sessionManager.getSessionId(), await running.stop());
      return;
    }
    live = ctx;
    unheld = true;
    unhold();
    await release();
    unlead();
    // pi's own signal handler can reach here inside the dispatch of the signal, before
    // Steery's listener ran ([`signals`]); one turn of the queue lets it. pi's document
    // always says it, `null` for none.
    await Promise.resolve();
    const signal = HARNESS === "pi" ? { signal: signals().by ?? null } : {};
    const id = moment("SessionEnd", document(ctx, { mode: ctx.mode, reason: closing(event, ctx), ...signal }));
    if (wanted && !door) dial();
    await awaited(id, FLUSH_MS);
    shut();
    forgotten(ctx.sessionManager.getSessionId());
  });

  // A session already under way that the loader moved onto this copy begins here, with
  // no SessionStart moment: nothing started.
  if (carried.ctx) begin(carried.ctx);

  return running;
}
