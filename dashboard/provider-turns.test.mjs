import assert from 'node:assert/strict';
import test from 'node:test';
import { buildProviderTurns, mainAgentTurns, mergeProviderEvents, pairToolCalls, subagentTurns, sumUsage } from './provider-turns.mjs';

let sequence = 0;
function ev(event, timestamp_ms, extra = {}) {
  sequence += 1;
  return { event_id: `e${sequence}`, event, timestamp_ms, session_key: 's1', ...extra };
}
const tool = (phase, timestamp_ms, tool_name, args, extra = {}) =>
  ev(phase, timestamp_ms, { tool_name, tool_arguments: JSON.stringify(args), ...extra });

test('mergeProviderEvents keeps each event once, oldest first', () => {
  const a = ev('Stop', 20);
  const b = ev('UserPromptSubmit', 10);
  const merged = mergeProviderEvents([a, b], [a], null, [ev('TurnUsage', 30)]);
  assert.deepEqual(merged.map(item => item.event), ['UserPromptSubmit', 'Stop', 'TurnUsage']);
});

test('sumUsage adds the disjoint input fields and output', () => {
  assert.equal(sumUsage([]), null);
  const usage = sumUsage([
    ev('TurnUsage', 1, { input_tokens: 10, cache_creation_input_tokens: 5, cache_read_input_tokens: 100, output_tokens: 7, total_tokens: 17 }),
    ev('TurnUsage', 2, { input_tokens: 1, output_tokens: -3 })
  ]);
  assert.deepEqual(usage, { fresh: 11, cacheWrite: 5, cacheRead: 100, output: 7, reported: 17, events: 2, total: 123 });
});

test('pairToolCalls matches Post to Pre by tool and arguments, then oldest first', () => {
  const calls = pairToolCalls([
    tool('PreToolUse', 1, 'Bash', { command: 'ls' }),
    tool('PreToolUse', 2, 'Bash', { command: 'pwd' }),
    tool('PostToolUse', 5, 'Bash', { command: 'pwd' }),
    tool('PostToolUseFailure', 9, 'Bash', { command: 'ls' }),
    tool('PreToolUse', 10, 'Read', { file: 'a' }),
    tool('PostToolUse', 12, 'Read', { file: 'other' }),
    tool('PostToolUse', 15, 'Edit', { file: 'b' })
  ]);
  assert.deepEqual(calls.map(call => [call.tool_name, call.startMs, call.duration, call.failed, Boolean(call.pre), Boolean(call.post)]), [
    ['Bash', 1, 8, true, true, true],
    ['Bash', 2, 3, false, true, true],
    ['Read', 10, 2, false, true, true],
    ['Edit', 15, null, false, false, true]
  ]);
});

test('pairToolCalls leaves a Pre without a Post open', () => {
  const [call] = pairToolCalls([tool('PreToolUse', 1, 'Bash', { command: 'sleep 9' })]);
  assert.equal(call.post, null);
  assert.equal(call.duration, null);
});

test('mainAgentTurns splits at prompts and keeps usage after a stop with its turn', () => {
  const { preamble, turns } = mainAgentTurns([
    ev('SessionStart', 1),
    ev('MemoryRetrieved', 2, { retrieved_memory_count: 1 }),
    ev('UserPromptSubmit', 10, { prompt_preview: 'first' }),
    ev('MemoryRetrieved', 11),
    tool('PreToolUse', 12, 'Bash', { command: 'ls' }),
    tool('PostToolUse', 13, 'Bash', { command: 'ls' }),
    ev('Stop', 20),
    ev('TurnUsage', 21, { input_tokens: 4, output_tokens: 1 }),
    ev('UserPromptSubmit', 30, { prompt_preview: 'second' }),
    ev('Stop', 35)
  ]);
  assert.deepEqual(preamble.map(item => item.event), ['SessionStart', 'MemoryRetrieved']);
  assert.equal(turns.length, 2);
  const [first, second] = turns;
  assert.equal(first.opener.prompt_preview, 'first');
  assert.equal(first.duration, 10);
  assert.equal(first.tools.length, 1);
  assert.equal(first.retrievals.length, 1);
  assert.equal(first.usage.total, 5);
  assert.deepEqual(first.timeline.map(item => item.kind), ['prompt', 'retrieval', 'tool', 'stop']);
  assert.equal(first.partial, false);
  assert.equal(second.usage, null);
});

test('mainAgentTurns marks activity before the first prompt as a partial turn', () => {
  const { preamble, turns } = mainAgentTurns([
    tool('PreToolUse', 1, 'Bash', { command: 'ls' }),
    ev('Stop', 2),
    ev('UserPromptSubmit', 5)
  ]);
  assert.equal(preamble.length, 0);
  assert.equal(turns.length, 2);
  assert.equal(turns[0].partial, true);
  assert.equal(turns[0].closer.event, 'Stop');
  assert.equal(turns[1].partial, false);
});

test('mainAgentTurns starts a turn when turn_id changes, even without a prompt', () => {
  const { turns } = mainAgentTurns([
    tool('PreToolUse', 1, 'Bash', { command: 'a' }, { turn_id: 't1' }),
    ev('Stop', 2, { turn_id: 't1' }),
    ev('TurnUsage', 3, { turn_id: 't1', input_tokens: 2 }),
    tool('PreToolUse', 4, 'Bash', { command: 'b' }, { turn_id: 't2' }),
    ev('Stop', 5, { turn_id: 't2' })
  ]);
  assert.deepEqual(turns.map(turn => [turn.turn_id, turn.partial, turn.closer?.event, turn.usage?.total ?? null]), [
    ['t1', true, 'Stop', 2],
    ['t2', true, 'Stop', null]
  ]);
});

test('mainAgentTurns flags turns older than the tool window', () => {
  const { turns } = mainAgentTurns([
    ev('UserPromptSubmit', 10),
    ev('Stop', 20),
    ev('UserPromptSubmit', 100),
    tool('PreToolUse', 110, 'Bash', { command: 'ls' })
  ], { windowStartMs: 50 });
  assert.deepEqual(turns.map(turn => turn.toolsTruncated), [true, false]);
});

test('subagentTurns treats each start as a new turn of the same agent', () => {
  const agent = { agent_id: 'a1', agent_type: 'Explore' };
  const turns = subagentTurns([
    ev('SubagentStart', 10, agent),
    tool('PreToolUse', 11, 'Bash', { command: 'ls' }, agent),
    ev('SubagentStart', 20, agent),
    tool('PreToolUse', 21, 'Read', { file: 'x' }, agent),
    tool('PostToolUse', 22, 'Read', { file: 'x' }, agent),
    ev('SubagentStop', 30, agent),
    ev('TurnUsage', 31, { ...agent, input_tokens: 8, output_tokens: 2 }),
    tool('PreToolUse', 40, 'Edit', { file: 'y' }, agent)
  ]);
  assert.deepEqual(turns.map(turn => [turn.startMs, turn.partial, Boolean(turn.closer), turn.tools.length, turn.usage?.total ?? null]), [
    [10, false, false, 1, null],
    [20, false, true, 1, 10],
    [40, true, false, 1, null]
  ]);
  assert.deepEqual(turns[1].timeline.map(item => item.kind), ['start', 'tool', 'stop']);
  assert.equal(turns[1].duration, 10);
});

test('buildProviderTurns separates subagent work from the main turn and links it back', () => {
  const now = 1_000_000;
  const agent = { agent_id: 'a1', agent_type: 'Explore' };
  const result = buildProviderTurns([
    ev('UserPromptSubmit', 100, { prompt_preview: 'review' }),
    tool('PreToolUse', 110, 'Bash', { command: 'cat' }),
    tool('PostToolUse', 111, 'Bash', { command: 'cat' }),
    ev('SubagentStart', 120, agent),
    tool('PreToolUse', 121, 'Bash', { command: 'ls' }, agent),
    tool('PostToolUse', 122, 'Bash', { command: 'ls' }, agent),
    ev('SubagentStop', 130, agent),
    ev('TurnUsage', 131, { ...agent, input_tokens: 9, output_tokens: 3 }),
    ev('Stop', 140),
    ev('TurnUsage', 141, { input_tokens: 50, output_tokens: 5 })
  ], { now });

  assert.equal(result.sessions.length, 1);
  const [main] = result.sessions[0].turns;
  assert.equal(main.state, 'completed');
  assert.deepEqual(main.tools.map(call => JSON.parse(call.pre.tool_arguments).command), ['cat']);
  assert.equal(main.usage.total, 55);
  assert.deepEqual(main.timeline.map(item => item.kind), ['prompt', 'tool', 'subagent', 'stop']);
  assert.equal(main.subagents.length, 1);

  assert.equal(result.agents.length, 1);
  const [sub] = result.agents;
  assert.equal(sub.agent_type, 'Explore');
  assert.equal(sub.state, 'completed');
  assert.equal(sub.usage.total, 12);
  assert.equal(sub.turns[0].parentTurnId, main.id);
  assert.deepEqual(sub.parentTurnIds, [main.id]);
  assert.equal(main.subagents[0].turnId, sub.turns[0].id);
});

test('buildProviderTurns sets open states and lists newest first', () => {
  const now = 10_000_000;
  const result = buildProviderTurns([
    ev('UserPromptSubmit', 100, { session_key: 'old' }),
    ev('UserPromptSubmit', 200, { session_key: 'old' }),
    ev('SessionEnd', 300, { session_key: 'old' }),
    ev('UserPromptSubmit', now - 1000, { session_key: 'live' }),
    ev('SubagentStart', now - 900, { session_key: 'live', agent_id: 'busy' }),
    ev('SubagentStart', now - 120_000, { session_key: 'live', agent_id: 'quiet' }),
    ev('SubagentStart', 150, { session_key: 'old', agent_id: 'gone' })
  ], { now, idleMs: 60_000 });

  assert.deepEqual(result.sessions.map(session => session.session_key), ['live', 'old']);
  assert.deepEqual(result.sessions[1].turns.map(turn => turn.state), ['ended', 'no_stop']);
  assert.equal(result.sessions[1].ended, true);
  assert.equal(result.sessions[0].turns[0].state, 'running');
  const byId = Object.fromEntries(result.agents.map(agent => [agent.agent_id, agent.state]));
  assert.deepEqual(byId, { busy: 'running', quiet: 'quiet', gone: 'ended' });
});

test('buildProviderTurns groups a subagent start without agent_id by its type', () => {
  const result = buildProviderTurns([
    ev('UserPromptSubmit', 1),
    ev('SubagentStart', 2, { agent_type: 'Plan' }),
    ev('SubagentStop', 3, { agent_type: 'Plan' })
  ], { now: 10 });
  assert.equal(result.sessions[0].turns[0].tools.length, 0);
  assert.deepEqual(result.agents.map(agent => [agent.key, agent.agent_id, agent.turns.length]), [['type:Plan', null, 1]]);
});
