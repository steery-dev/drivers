// Steery's extension, written by `steery setup` and replaced by it on every upgrade.
//
// omp loads this file once per process and keeps it until it restarts; pi loads it
// afresh for every extension runtime it builds — at start, at `/reload`, and for every
// session it replaces the last one with — and tears that runtime down with
// `session_shutdown`. Either way it holds nothing of Steery's own: it runs Steery's
// extension (`extension.ts`) — the copy the runtime names in the file below, one per
// version — and moves every session it runs onto a newer copy as soon as the runtime
// names one. A session opened before an upgrade so has what the upgrade brought, the
// door its person's words come in by above all, without a restart. The harness holds
// this file's handlers, one per place a copy gave one, and each hands its event to
// whichever copy runs now. What the harness keeps for the life of its runtime — the
// command-line flags and slash commands a copy names in its `surfaces` — is registered
// here, once, and a command hands its call to whichever copy runs now, so a newer copy
// never registers anything twice. The harness's terminal toolkit is imported here and
// handed to every copy: each harness resolves its own for the file it loads as an
// extension, and omp for no file outside its tree, which is where a copy is. A copy that
// will not load leaves the one that runs; with none named, the harness runs as it would
// without this file.

import { readFileSync } from "node:fs";

/** The file naming the copy of Steery's extension to run here: one line, its path. */
const CURRENT: string = "__STEERY_CURRENT__";

/** How often the name is read again: how soon a session moves onto a newer copy. */
const EVERY_MS = 5000;

/** How long a copy has to let go of a session before the next takes it anyway. */
const STOP_MS = 5000;

type Handler = (event: unknown, ctx: unknown) => unknown;

/** A command-line flag a copy names, registered by this file for the harness's runtime. */
type Flag = { name: string; description?: string; type: "string" | "boolean" };

/** A slash command a copy names, registered by this file for the harness's runtime. */
type Command = { name: string; description?: string };

/** The harness's API for one session, as far as this file reads it. */
type Api = {
  on(event: string, handler: Handler): void;
  registerFlag?(name: string, options: { description?: string; type: "string" | "boolean" }): void;
  registerCommand?(name: string, options: { description?: string; handler(args: string, ctx: unknown): Promise<void> }): void;
};

/**
 * A copy running in one session: stopped, it says what the next copy carries on from;
 * given one of its commands, it runs it.
 */
type Running = { stop(): unknown; command?(name: string, args: string, ctx: unknown): Promise<void> };

/**
 * Steery's extension: its default export starts a copy in a session, one under way when
 * `ctx` is given; `surfaces` names the flags and commands it needs the harness to hold.
 */
type Extension = {
  default(
    pi: Api,
    carried: { ctx?: unknown; kept?: unknown; tui?: unknown; models?: unknown },
  ): Running | undefined | Promise<Running | undefined>;
  surfaces?: { flags?: Flag[]; commands?: Command[] };
};

/** One session of this process, in the order the harness started them: a lead before its sub-agents. */
type Session = {
  pi: Api;
  /** The running copy's handlers, by event, in the order it gave them. */
  handlers: Map<string, Handler[]>;
  /** How many handlers the harness holds for each event: one per place, given once and kept. */
  places: Map<string, number>;
  started: boolean;
  running?: Running;
  /** The context the harness last handed this session: where a copy started later takes it up. */
  ctx?: unknown;
};

const sessions: Session[] = [];
/** The copy running now, and the path it was named by. */
let running: { path: string; module: Extension } | undefined;
/** A path whose copy would not load: not tried again until another is named. */
let refused: string | undefined;
/** One move at a time; an event that arrives during one goes to the copy it moved to. */
let moving: Promise<void> = Promise.resolve();
let shifting = false;
let watching: NodeJS.Timeout | undefined;
/** The harness's terminal toolkit, imported once, or nothing where this harness has none. */
let toolkit: unknown;
/** The harness's own model calls, imported once ([`harnessModels`]). */
let models: unknown;

/**
 * Not static imports, as with the toolkit: a harness that cannot resolve one would refuse
 * this whole file, and Steery fails open.
 *
 * What of the harness's own packages a conversation's recap is written with, by the names
 * both harnesses resolve for an extension: `convertToLlm` from its coding agent and
 * `completeSimple` from its model package (omp maps both to its own). Either missing is
 * no recap here, never a refused file.
 */
async function harnessModels(): Promise<unknown> {
  const loaded: Record<string, unknown> = {};
  try {
    loaded.convertToLlm = ((await import("@earendil-works/pi-coding-agent")) as Record<string, unknown>).convertToLlm;
  } catch {
    // No recap of its own here.
  }
  try {
    loaded.completeSimple = ((await import("@earendil-works/pi-ai")) as Record<string, unknown>).completeSimple;
  } catch {
    // pi's registry makes the request itself.
  }
  return loaded;
}

/**
 * The toolkit the harness's own extensions draw with, by the name both harnesses resolve
 * for an extension: pi's own package, and omp's in its place.
 */
async function terminalToolkit(): Promise<unknown> {
  try {
    // Not a static import: a harness that cannot resolve it would refuse this whole file,
    // and Steery fails open. Without it, the copy draws no card.
    return await import("@earendil-works/pi-tui");
  } catch {
    return undefined;
  }
}

function named(): string | undefined {
  try {
    return readFileSync(CURRENT, "utf8").trim() || undefined;
  } catch {
    return undefined;
  }
}

function queued(step: () => Promise<void>): Promise<void> {
  moving = moving.then(step).catch(() => {});
  return moving;
}

/** A promise, or nothing once `ms` passed. */
async function within<T>(promise: Promise<T> | T, ms: number): Promise<T | undefined> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const late = new Promise<undefined>((resolve) => {
    timer = setTimeout(() => resolve(undefined), ms);
  });
  try {
    return await Promise.race([Promise.resolve(promise), late]);
  } finally {
    clearTimeout(timer);
  }
}

/** The session's API as a copy sees it: the harness's own, but every handler given here. */
function api(session: Session): Api {
  return new Proxy(session.pi, {
    get(target, key) {
      if (key === "on") return (event: string, handler: Handler) => on(session, event, handler);
      const value: unknown = Reflect.get(target, key);
      return typeof value === "function" ? value.bind(target) : value;
    },
  });
}

/** A copy's handler, in the next place the harness holds for its event: given to the harness once, for good. */
function on(session: Session, event: string, handler: Handler): void {
  const list = session.handlers.get(event) ?? [];
  session.handlers.set(event, list);
  const place = list.push(handler) - 1;
  if ((session.places.get(event) ?? 0) > place) return;
  session.places.set(event, place + 1);
  session.pi.on(event, (payload, ctx) => {
    if (ctx !== undefined) session.ctx = ctx;
    const call = () => session.handlers.get(event)?.[place]?.(payload, ctx);
    return shifting ? moving.then(call) : call();
  });
}

/** Start the running copy in a session, carrying on from what the copy before it kept. */
async function start(session: Session, kept: unknown): Promise<void> {
  if (!running) return;
  try {
    session.running =
      (await running.module.default(api(session), { ctx: session.ctx, kept, tui: toolkit, models })) ?? undefined;
    session.started = true;
  } catch {
    session.handlers = new Map();
    session.started = false;
  }
}

/** Stop a session's copy, and say what it kept. */
async function stop(session: Session): Promise<unknown> {
  const was = session.running;
  session.running = undefined;
  session.started = false;
  try {
    return was ? await within(was.stop(), STOP_MS) : undefined;
  } catch {
    return undefined;
  } finally {
    session.handlers = new Map();
  }
}

/**
 * Move every session onto the copy named now, where it is another and loads: each copy
 * stopped, the last started first, then each session started on the new one in the order
 * the harness started them. Otherwise a session not running starts on the copy there is.
 */
async function follow(): Promise<void> {
  const path = named();
  if (path && path !== running?.path && path !== refused) {
    let module: Extension | undefined;
    try {
      module = (await import(path)) as Extension;
    } catch {
      refused = path;
    }
    if (module) {
      shifting = true;
      try {
        const kept = new Map<Session, unknown>();
        for (const session of [...sessions].reverse()) kept.set(session, await stop(session));
        running = { path, module };
        refused = undefined;
        for (const session of sessions) await start(session, kept.get(session));
      } finally {
        shifting = false;
      }
      return;
    }
  }
  for (const session of sessions) if (!session.started) await start(session, undefined);
}

/**
 * Whether this machine's runtime started this process to write no conversation — a
 * model catalog, a reader — as `STEERY_STARTED` says it (`lineage.rs`): believed only of
 * the process that set it, this one's parent, and never of a parent of pid 1.
 */
function startedForNothing(): boolean {
  const value = process.env.STEERY_STARTED ?? "";
  const at = value.indexOf(":");
  if (at < 0) return false;
  const by = Number(value.slice(0, at));
  return process.ppid !== 1 && by === process.ppid && value.slice(at + 1) === "nothing";
}

export default async function steery(pi: Api): Promise<void> {
  // A process that writes no conversation runs none of Steery: it has no conversation
  // for a door to say, and pi lists its models with every extension loaded.
  if (startedForNothing()) return;
  const session: Session = { pi, handlers: new Map(), places: new Map(), started: false };
  sessions.push(session);
  toolkit ??= await terminalToolkit();
  models ??= await harnessModels();
  // Where a copy started after the session did takes it up. The name is watched only
  // while a session runs here: a harness may load this file and start none — pi does to
  // list its models — and the watch ends with the last session, which a pi runtime torn
  // down at `/reload` or a session's replacement never outlives.
  pi.on("session_start", (_event, ctx) => {
    session.ctx = ctx;
    if (!watching) {
      watching = setInterval(() => void queued(follow), EVERY_MS);
      watching.unref?.();
    }
  });
  pi.on("session_shutdown", () => {
    const at = sessions.indexOf(session);
    if (at >= 0) sessions.splice(at, 1);
    if (sessions.length === 0) {
      clearInterval(watching);
      watching = undefined;
    }
  });
  await queued(follow);
  // What the copy running now needs the harness to hold for this runtime's life. pi
  // refuses a flag nobody registered before anything starts, and waits for this factory,
  // so a flag registered here is one a run may be given. A command finds whichever copy
  // runs when it is called, once any move under way is done.
  const surfaces = running?.module.surfaces;
  for (const flag of surfaces?.flags ?? []) {
    try {
      pi.registerFlag?.(flag.name, { description: flag.description, type: flag.type });
    } catch {
      // The harness runs without it, as without Steery.
    }
  }
  for (const command of surfaces?.commands ?? []) {
    try {
      pi.registerCommand?.(command.name, {
        description: command.description,
        handler: async (args, ctx) => {
          if (shifting) await moving;
          await session.running?.command?.(command.name, args, ctx);
        },
      });
    } catch {
      // The harness runs without it, as without Steery.
    }
  }
}
