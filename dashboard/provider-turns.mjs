// Pure helpers that rebuild provider telemetry into turns. The main agent and
// each subagent get their own turns. Events without agent_id belong to the
// main agent; events with one belong to that subagent. Nothing here touches
// the DOM, so every rule is covered by provider-turns.test.mjs.

export const TURN_IDLE_MS = 30 * 60 * 1000;

const PROMPT = /^(UserPromptSubmit|BeforeAgent)$/;
const STOP = /^(Stop|AfterAgent)$/;
const TOOL_PRE = /^(PreToolUse|BeforeTool)$/;
const TOOL_POST = /^(PostToolUse|PostToolUseFailure|AfterTool)$/;
const SESSION = /^(SessionStart|SessionEnd)$/;

const at = event => Number(event?.timestamp_ms || 0);
const byTime = (a, b) => at(a) - at(b) || String(a.event_id || '').localeCompare(String(b.event_id || ''));
const isRetrieval = event => event.event === 'MemoryRetrieved' || Boolean(event.retrieved_memory_details);

// Joins the retained window, milestone and subagent lists into one list,
// oldest first, with each event_id kept once.
export function mergeProviderEvents(...lists) {
  const seen = new Map();
  for (const list of lists) {
    for (const event of Array.isArray(list) ? list : []) {
      if (!event) continue;
      const key = event.event_id || `${event.event}:${at(event)}:${event.session_key || ''}`;
      if (!seen.has(key)) seen.set(key, event);
    }
  }
  return [...seen.values()].sort(byTime);
}

// Sums usage events with the disjoint input fields: uncached input, cache
// writes and cache reads. Returns null when no usage event is given.
export function sumUsage(usageEvents = []) {
  if (!usageEvents.length) return null;
  const sum = field => usageEvents.reduce((total, event) => total + (Number.isFinite(event[field]) && event[field] >= 0 ? event[field] : 0), 0);
  const usage = {
    fresh: sum('input_tokens'),
    cacheWrite: sum('cache_creation_input_tokens'),
    cacheRead: sum('cache_read_input_tokens'),
    output: sum('output_tokens'),
    reported: sum('total_tokens'),
    events: usageEvents.length
  };
  usage.total = usage.fresh + usage.cacheWrite + usage.cacheRead + usage.output;
  return usage;
}

// Pairs Pre and Post tool events into calls, in time order. A Post closes the
// oldest open Pre of the same tool with the same arguments, and otherwise the
// oldest open Pre of the same tool. A Post with no Pre (evicted) is a call
// with pre null; a Pre with no Post is a call with post null.
export function pairToolCalls(events = []) {
  const calls = [];
  const open = [];
  for (const event of events.slice().sort(byTime)) {
    if (TOOL_PRE.test(event.event)) {
      const call = { pre: event, post: null, tool_name: event.tool_name || null, startMs: at(event), endMs: null, duration: null, failed: false };
      calls.push(call);
      open.push(call);
    } else if (TOOL_POST.test(event.event)) {
      const sameTool = open.filter(call => call.tool_name === (event.tool_name || null));
      const call = sameTool.find(item => item.pre.tool_arguments === event.tool_arguments) || sameTool[0];
      const failed = event.event === 'PostToolUseFailure';
      if (call) {
        open.splice(open.indexOf(call), 1);
        call.post = event;
        call.endMs = at(event);
        call.duration = Math.max(0, at(event) - call.startMs);
        call.failed = failed;
      } else {
        calls.push({ pre: null, post: event, tool_name: event.tool_name || null, startMs: at(event), endMs: at(event), duration: null, failed });
      }
    }
  }
  return calls.sort((a, b) => a.startMs - b.startMs);
}

function newTurn(sessionKey, agentId, opener) {
  return { session_key: sessionKey, agent_id: agentId, turn_id: opener?.turn_id || null, opener, closer: null, events: opener ? [opener] : [] };
}

// Finishes a raw turn: pairs tools, sums usage, builds the timeline and sets
// timing. The state is set by the caller, which knows what came after.
function finishTurn(raw, { windowStartMs = 0, partial = false } = {}) {
  const events = raw.events.slice().sort(byTime);
  const usageEvents = events.filter(event => event.event === 'TurnUsage');
  const tools = pairToolCalls(events);
  const timeline = [];
  for (const event of events) {
    if (raw.opener === event) timeline.push({ kind: raw.agent_id ? 'start' : 'prompt', at: at(event), event });
    else if (raw.closer === event) timeline.push({ kind: 'stop', at: at(event), event });
    else if (isRetrieval(event)) timeline.push({ kind: 'retrieval', at: at(event), event });
    else if (TOOL_PRE.test(event.event) || TOOL_POST.test(event.event) || event.event === 'TurnUsage') continue;
    else timeline.push({ kind: 'other', at: at(event), event });
  }
  tools.forEach(call => timeline.push({ kind: 'tool', at: call.startMs, call }));
  timeline.sort((a, b) => a.at - b.at);
  const startMs = events.length ? at(events[0]) : 0;
  const lastSeenMs = events.length ? at(events.at(-1)) : 0;
  return {
    id: `${raw.agent_id || 'main'}:${raw.session_key}:${raw.opener?.event_id || events[0]?.event_id || startMs}`,
    session_key: raw.session_key,
    agent_id: raw.agent_id,
    agent_type: events.find(event => event.agent_type)?.agent_type || null,
    turn_id: raw.turn_id || events.find(event => event.turn_id)?.turn_id || null,
    opener: raw.opener,
    closer: raw.closer,
    partial: partial || !raw.opener,
    toolsTruncated: Boolean(windowStartMs) && startMs < windowStartMs,
    tools,
    retrievals: events.filter(isRetrieval),
    usage: sumUsage(usageEvents),
    timeline,
    subagents: [],
    startMs,
    endMs: raw.closer ? at(raw.closer) : null,
    lastSeenMs,
    duration: raw.opener && raw.closer ? Math.max(0, at(raw.closer) - at(raw.opener)) : null,
    state: null
  };
}

// The state of a turn with no stop: superseded when a later turn followed it,
// ended when its session ended after it, quiet when idle too long, otherwise
// running.
function openState(turn, { superseded, sessionEndMs, now, idleMs }) {
  if (turn.closer) return 'completed';
  if (superseded) return 'no_stop';
  if (sessionEndMs != null && sessionEndMs >= turn.lastSeenMs) return 'ended';
  if (now - turn.lastSeenMs > idleMs) return 'quiet';
  return 'running';
}

// Splits one session's main-agent events into turns, oldest first. A prompt
// starts a turn; so does a change of turn_id when both sides carry one. Usage
// and other events after a stop stay with that turn until the next prompt.
// Events before the first prompt form a partial turn, except session events
// and anything after a SessionStart, which are the session preamble.
export function mainAgentTurns(sessionEvents = [], options = {}) {
  const sessionKey = sessionEvents[0]?.session_key || null;
  const preamble = [];
  const raws = [];
  let current = null;
  let started = false;
  for (const event of sessionEvents.slice().sort(byTime)) {
    if (SESSION.test(event.event)) {
      if (event.event === 'SessionStart') started = true;
      preamble.push(event);
      continue;
    }
    const turnChanged = current && event.turn_id && current.turn_id && event.turn_id !== current.turn_id;
    const prompt = PROMPT.test(event.event);
    if (prompt || turnChanged) {
      current = newTurn(sessionKey, null, prompt ? event : null);
      current.turn_id ||= event.turn_id || null;
      raws.push(current);
      if (prompt) continue;
    }
    if (!current) {
      if (started) { preamble.push(event); continue; }
      current = newTurn(sessionKey, null, null);
      current.turn_id = event.turn_id || null;
      raws.push(current);
    }
    current.turn_id ||= event.turn_id || null;
    current.events.push(event);
    if (STOP.test(event.event) && !current.closer) current.closer = event;
  }
  return { preamble, turns: raws.map(raw => finishTurn(raw, options)) };
}

// Splits one subagent's events into turns, oldest first. SubagentStart opens a
// turn. SubagentStop closes it, and usage right after the stop stays with it.
// Other activity after a stop, or before any start, opens a partial turn. A
// start that arrives while a turn is open marks that turn as no_stop.
export function subagentTurns(agentEvents = [], options = {}) {
  const raws = [];
  let current = null;
  for (const event of agentEvents.slice().sort(byTime)) {
    if (event.event === 'SubagentStart') {
      current = newTurn(event.session_key || null, event.agent_id || null, event);
      raws.push(current);
      continue;
    }
    const afterStop = current?.closer && event.event !== 'TurnUsage';
    if (!current || afterStop) {
      current = newTurn(event.session_key || null, event.agent_id || null, null);
      raws.push(current);
    }
    current.events.push(event);
    if (event.event === 'SubagentStop' && !current.closer) current.closer = event;
  }
  return raws.map(raw => finishTurn(raw, options));
}

const sessionEnd = events => {
  const ends = events.filter(event => event.event === 'SessionEnd').map(at);
  return ends.length ? Math.max(...ends) : null;
};

// Rebuilds a provider's events into main-agent sessions and subagents, both
// newest first, with turns newest first. Each main turn lists the subagent
// turns that started inside it, and gets a 'subagent' timeline item for each.
// Options: windowStartMs (oldest retained window event, so older turns are
// marked toolsTruncated), now, idleMs.
export function buildProviderTurns(events = [], { windowStartMs = 0, now = Date.now(), idleMs = TURN_IDLE_MS } = {}) {
  const options = { windowStartMs };
  const sorted = events.slice().sort(byTime);
  const mainBySession = new Map();
  const byAgent = new Map();
  for (const event of sorted) {
    const agentKey = event.agent_id || (/^Subagent(Start|Stop)$/.test(event.event) ? `type:${event.agent_type || 'subagent'}` : null);
    const map = agentKey ? byAgent : mainBySession;
    const key = agentKey || event.session_key || 'unknown';
    if (!map.has(key)) map.set(key, []);
    map.get(key).push(event);
  }
  const endsBySession = new Map();
  for (const [key, list] of mainBySession) endsBySession.set(key, sessionEnd(list));

  const settle = (turns, sessionEndFor) => turns.forEach((turn, index) => {
    turn.state = openState(turn, { superseded: index < turns.length - 1, sessionEndMs: sessionEndFor(turn), now, idleMs });
  });

  const agents = [...byAgent].map(([key, list]) => {
    const turns = subagentTurns(list, options);
    settle(turns, turn => endsBySession.get(turn.session_key) ?? null);
    const known = list.find(event => event.agent_id);
    return {
      agent_id: known?.agent_id || null,
      key,
      agent_type: list.find(event => event.agent_type)?.agent_type || null,
      session_key: list.find(event => event.session_key)?.session_key || null,
      turns,
      usage: sumUsage(list.filter(event => event.event === 'TurnUsage')),
      firstSeenMs: at(list[0]),
      lastSeenMs: at(list.at(-1)),
      state: turns.at(-1)?.state || null,
      parentTurnIds: []
    };
  });

  const sessions = [...mainBySession].map(([key, list]) => {
    const { preamble, turns } = mainAgentTurns(list, options);
    settle(turns, () => endsBySession.get(key) ?? null);
    return { session_key: key, preamble, turns, ended: endsBySession.get(key) != null, firstSeenMs: at(list[0]), lastSeenMs: at(list.at(-1)) };
  });

  // Attach each subagent turn to the main turn of its session that was going
  // on when it started: the latest main turn that began at or before it.
  const sessionByKey = new Map(sessions.map(session => [session.session_key, session]));
  for (const agent of agents) {
    for (const turn of agent.turns) {
      const session = sessionByKey.get(turn.session_key);
      const parent = session?.turns.filter(main => main.startMs <= turn.startMs).at(-1);
      if (!parent) continue;
      turn.parentTurnId = parent.id;
      parent.subagents.push({ agentKey: agent.key, agent_id: agent.agent_id, agent_type: agent.agent_type, turnId: turn.id, state: turn.state });
      parent.timeline.push({ kind: 'subagent', at: turn.startMs, agentKey: agent.key, turnId: turn.id });
      if (!agent.parentTurnIds.includes(parent.id)) agent.parentTurnIds.push(parent.id);
    }
  }
  sessions.forEach(session => session.turns.forEach(turn => turn.timeline.sort((a, b) => a.at - b.at)));

  const newestFirst = (a, b) => b.lastSeenMs - a.lastSeenMs;
  sessions.forEach(session => session.turns.reverse());
  agents.forEach(agent => agent.turns.reverse());
  return { sessions: sessions.sort(newestFirst), agents: agents.sort(newestFirst) };
}
