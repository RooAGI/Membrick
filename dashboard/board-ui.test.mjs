import assert from 'node:assert/strict';
import test from 'node:test';
import { escapeHtml, mergeBoardPosts, parseBoardLinkTarget, rollupTasks, tasksByRecentActivity } from './board-ui.mjs';

// Fixtures shaped like the memory-client-sdk-rollout board.
const BOARD = 'board:mcp:/ws:rollout';
const GROUPS = {
  kind: 'post_groups',
  post_template_ids: ['task'],
  group_by: 'status',
  groups: [
    { value: 'ready', label: 'Ready' },
    { value: 'running', label: 'In progress' },
    { value: 'review', label: 'In review' },
    { value: 'changes_requested', label: 'Changes requested' },
    { value: 'done', label: 'Done' }
  ]
};
const ref = sequence => `${BOARD}:post:${String(sequence).padStart(20, '0')}`;
function task(sequence, status, links = [], extra = {}) {
  return {
    sequence,
    post_id: ref(sequence),
    post_template_id: 'task',
    title: `Post ${sequence}`,
    created_at: `2026-10-27T00:00:${String(sequence).padStart(2, '0')}Z`,
    fields: { status, ...extra },
    links: links.map(([relation, target]) => ({ relation, target: typeof target === 'number' ? ref(target) : target }))
  };
}
const bySequence = result => new Map(result.tasks.map(entry => [entry.sequence, entry]));

test('merges oldest and latest board pages without duplicates in sequence order', () => {
  const result = mergeBoardPosts(
    [{ sequence: 1, title: 'first' }, { sequence: 2, title: 'old copy' }],
    [{ sequence: 2, title: 'latest copy' }, { sequence: 4, title: 'newest' }]
  );

  assert.deepEqual(result.map(post => post.sequence), [1, 2, 4]);
  assert.equal(result[1].title, 'latest copy');
});

test('parses board and post links, including malformed post sequence values', () => {
  assert.deepEqual(parseBoardLinkTarget('board:workspace:project'), {
    kind: 'board', boardId: 'board:workspace:project', sequence: null
  });
  assert.deepEqual(parseBoardLinkTarget('board:workspace:tasks:post:12'), {
    kind: 'post', boardId: 'board:workspace:tasks', sequence: 12
  });
  assert.deepEqual(parseBoardLinkTarget('board:workspace:tasks:post:nope'), {
    kind: 'post', boardId: 'board:workspace:tasks', sequence: null
  });
});

test('classifies external and opaque references without treating them as boards', () => {
  assert.equal(parseBoardLinkTarget('https://example.com/task').kind, 'external');
  assert.equal(parseBoardLinkTarget('ticket-123').kind, 'reference');
  assert.equal(parseBoardLinkTarget('local-board', ['local-board']).kind, 'board');
});

test('a task verified by a later post rolls up to done', () => {
  const result = rollupTasks([
    task(18, 'ready', [['tracks', BOARD]]),
    task(19, 'ready', [['tracks', BOARD]]),
    task(28, 'review', [['tracks', 18], ['tracks', 19]], { assignee: 'worker' }),
    task(30, 'done', [['verifies', 18], ['verifies', 19]], { assignee: 'orchestrator' })
  ], BOARD, GROUPS);
  const tasks = bySequence(result);

  assert.deepEqual([...tasks.keys()], [18, 19]);
  assert.equal(tasks.get(18).status, 'done');
  assert.equal(tasks.get(18).statusLabel, 'Done');
  assert.equal(tasks.get(18).assignee, 'orchestrator');
  assert.deepEqual(tasks.get(18).updates, [28, 30]);
  assert.deepEqual(tasks.get(18).latest, { sequence: 30, title: 'Post 30', createdAt: '2026-10-27T00:00:30Z', status: 'done' });
  assert.equal(result.groups.find(group => group.value === 'done').count, 2);
  assert.equal(result.groups.find(group => group.value === 'ready').count, 0);
});

test('a review chain resolves to its root task', () => {
  const result = rollupTasks([
    task(42, 'ready'),
    task(68, 'review', [['evidence_for', 42]]),
    task(69, 'changes_requested', [['reviews', 68]]),
    task(70, 'review', [['evidence_for', 69]]),
    task(71, 'done', [['reviews', 70]])
  ], BOARD, GROUPS);

  assert.equal(result.total, 1);
  assert.equal(result.tasks[0].sequence, 42);
  assert.equal(result.tasks[0].status, 'done');
  assert.deepEqual(result.tasks[0].updates, [68, 69, 70, 71]);
  assert.deepEqual(result.rootsOf.get(71), [42]);
});

test('depends_on records a dependency without changing status', () => {
  const result = rollupTasks([
    task(35, 'ready'),
    task(36, 'blocked', [['depends_on', 35]]),
    task(53, 'running', [['tracks', 35]]),
    task(60, 'done', [['depends_on', 36]])
  ], BOARD, GROUPS);
  const tasks = bySequence(result);

  assert.deepEqual([...tasks.keys()], [35, 36, 60]);
  assert.equal(tasks.get(35).status, 'running');
  assert.equal(tasks.get(36).status, 'blocked');
  assert.deepEqual(tasks.get(36).dependsOn, [35]);
  assert.deepEqual(tasks.get(36).updates, []);
  // blocked is not declared by the component, so it counts as Other.
  assert.equal(tasks.get(36).group, null);
  assert.equal(result.other.count, 1);
  assert.equal(result.other.tasks[0].sequence, 36);
});

test('a post linking to several roots updates every target', () => {
  const result = rollupTasks([
    task(20, 'ready'),
    task(21, 'ready'),
    task(27, 'review', [['evidence_for', 20], ['evidence_for', 21]], { assignee: 'rust' }),
    task(31, 'running', [['tracks', 21]]),
    task(42, 'blocked', [['extends', 21]])
  ], BOARD, GROUPS);
  const tasks = bySequence(result);

  assert.equal(tasks.get(20).status, 'review');
  assert.equal(tasks.get(21).status, 'running');
  // extends relates a separate task; it is not an update.
  assert.equal(tasks.get(42).status, 'blocked');
  assert.equal(tasks.get(21).assignee, 'rust');
  assert.deepEqual(result.rootsOf.get(27), [20, 21]);
});

test('tasks sort by most recent activity first, across statuses', () => {
  const result = rollupTasks([
    task(1, 'ready'),
    task(2, 'ready'),
    task(3, 'running'),
    task(4, 'done', [['verifies', 1]]),
    task(5, 'review', [['tracks', 3]])
  ], BOARD, GROUPS);
  const before = result.tasks.map(entry => entry.sequence);
  // Task 3 was updated last (#5), then task 1 was finished (#4), then task 2.
  assert.deepEqual(tasksByRecentActivity(result.tasks).map(entry => entry.sequence), [3, 1, 2]);
  assert.deepEqual(result.tasks.map(entry => entry.sequence), before, 'input order is untouched');
  assert.deepEqual(tasksByRecentActivity([]), []);
  // Equal latest sequences fall back to the newer task first.
  assert.deepEqual(tasksByRecentActivity([{ sequence: 1, latest: { sequence: 9 } }, { sequence: 2, latest: { sequence: 9 } }]).map(entry => entry.sequence), [2, 1]);
});

test('posts without update links stay their own task, others are ignored', () => {
  const result = rollupTasks([
    task(17, 'done'),
    task(18, 'ready', [['tracks', BOARD]]),
    task(19, 'ready', [['tracks', 'board:mcp:/ws:other:post:1']]),
    task(20, 'done', [['tracks', 25]]),
    { sequence: 21, title: 'note', fields: { status: 'done' }, links: [{ relation: 'tracks', target: ref(18) }] },
    task(22, '', [['tracks', 18]]),
    { ...task(23, 'done', [['tracks', 18]]), post_template_id: 'note' }
  ], BOARD, GROUPS);

  assert.deepEqual(result.tasks.map(entry => [entry.sequence, entry.status]), [[17, 'done'], [18, 'ready'], [19, 'ready'], [20, 'done']]);
});

test('escapes agent-authored text before the dashboard inserts it as HTML', () => {
  assert.equal(escapeHtml(`<script a='"'>&`), '&lt;script a=&#39;&quot;&#39;&gt;&amp;');
});

/* ── Flow view helpers ── */

const STAGES = [
  { id: 'implementation', name: 'Implementation', description: 'Build it.' },
  { id: 'independent_review', name: 'Independent review' },
  { id: 'unit_tests', name: 'Unit tests' }
];

test('flow stage steps mark earlier stages done, the current one current and later ones upcoming', () => {
  const steps = flowStageSteps(STAGES, 'independent_review', 'active');
  assert.deepEqual(steps.map(step => step.state), ['done', 'current', 'upcoming']);
  assert.equal(steps[0].description, 'Build it.');
  assert.deepEqual(flowStageSteps(STAGES, 'unit_tests', 'completed').map(step => step.state), ['done', 'done', 'done']);
  const completedLater = flowStageSteps(STAGES, 'implementation', 'active', [{ kind: 'stage_completed', data: { stage: 'unit_tests' } }]);
  assert.deepEqual(completedLater.map(step => step.state), ['current', 'upcoming', 'done']);
  const unknown = flowStageSteps(STAGES, 'hotfix', 'active');
  assert.deepEqual(unknown.map(step => [step.id, step.state]).at(-1), ['hotfix', 'current']);
});

test('flow tracked work resolves references to root tasks through the rollup', () => {
  const posts = [
    task(35, 'ready'),
    task(36, 'ready'),
    task(41, 'review', [['evidence_for', 35]]),
    task(50, 'done', [['verifies', 36]]),
    { sequence: 51, post_id: ref(51), title: 'Untemplated note', fields: {}, links: [] }
  ];
  const rollup = rollupTasks(posts, BOARD, GROUPS);
  const references = [
    { id: ref(35), kind: 'post', relation: 'tracks' },
    { id: ref(35), kind: 'post', relation: 'tracks' },
    { id: ref(41), kind: 'post', relation: 'evidence_for' },
    { id: ref(50), kind: 'post', relation: 'evidence_for' },
    { id: ref(51), kind: 'post', relation: 'tracks' },
    { id: 'board:mcp:/ws:other:post:3', kind: 'post', relation: 'tracks' }
  ];
  const work = flowTrackedWork(references, rollup, BOARD);
  assert.deepEqual(work.tasks.map(item => [item.sequence, item.status, item.direct]), [[35, 'review', true], [36, 'done', false]]);
  assert.deepEqual(work.tasks[0].references, [35, 41]);
  assert.deepEqual(work.otherReferences.map(item => item.id), [ref(51), 'board:mcp:/ws:other:post:3']);
  assert.deepEqual(work.groups.map(group => [group.value, group.count]), rollup.groups.map(group => [group.value, group.value === 'review' || group.value === 'done' ? 1 : 0]));
  assert.equal(work.other.count, 0);
  assert.equal(work.total, 2);
});

test('flow tracked work groups match the board rollup groups, with undeclared statuses as Other', () => {
  const component = { group_by: 'status', groups: [{ value: 'ready', label: 'Ready' }, { value: 'changes_requested', label: 'Changes requested' }, { value: 'done', label: 'Done' }] };
  const posts = [task(1, 'ready'), task(2, 'changes_requested'), task(3, 'mystery'), task(4, 'done'), task(5, 'ready')];
  const rollup = rollupTasks(posts, BOARD, component);
  const work = flowTrackedWork([1, 2, 3, 4].map(sequence => ({ id: ref(sequence), kind: 'post', relation: 'tracks' })), rollup, BOARD);
  assert.deepEqual(work.groups.map(group => [group.label, group.count]), [['Ready', 1], ['Changes requested', 1], ['Done', 1]]);
  assert.deepEqual([work.other.label, work.other.count, work.other.tasks.map(item => item.sequence)], ['Other', 1, [3]]);
  // Tracking every task gives exactly the board's own counts.
  const all = flowTrackedWork(posts.map(post => ({ id: ref(post.sequence), kind: 'post', relation: 'tracks' })), rollup, BOARD);
  assert.deepEqual(all.groups.map(group => group.count), rollup.groups.map(group => group.count));
  assert.equal(all.other.count, rollup.other.count);
  // No declared groups: every task is Other, as on the board.
  const bare = flowTrackedWork([{ id: ref(1), kind: 'post', relation: 'tracks' }], rollupTasks(posts, BOARD, {}), BOARD);
  assert.deepEqual([bare.groups.length, bare.other.count], [0, 1]);
});

test('flow post labels name the template and title', () => {
  assert.equal(flowPostLabel({ post_template_id: 'task', title: 'Production resident path', sequence: 35 }, 35), 'Task #35 · Production resident path');
  assert.equal(flowPostLabel(null, 7), 'Post #7');
});

test('flow event summaries read as sentences and never contain JSON', () => {
  const stageName = id => ({ independent_review: 'Independent review', unit_tests: 'Unit tests' }[id] || id);
  const moved = flowEventSummary({ kind: 'stage_changed', data: { coordinator_fence: 1, stage: 'independent_review', reviewer: '/root/r', approved: true } }, stageName);
  assert.equal(moved.text, 'moved the flow to Independent review');
  assert.deepEqual(moved.details.map(([label, text]) => [label, text]), [['reviewer', '/root/r'], ['approved', 'yes']]);
  assert.equal(flowEventSummary({ kind: 'stage_completed', data: { stage: 'unit_tests' } }, stageName).text, 'completed stage Unit tests');
  const added = flowEventSummary({ kind: 'reference_added', data: { coordinator_fence: 1, reference: { id: ref(35), kind: 'post', relation: 'tracks' } } });
  assert.equal(added.text, 'added');
  assert.deepEqual(added.reference, { id: ref(35), kind: 'post', relation: 'tracks' });
  assert.deepEqual(added.details, []);
  assert.equal(flowEventSummary({ kind: 'review_approved', task_id: 'T1', data: {} }).text, 'approved task T1');
  assert.equal(flowEventSummary({ kind: 'blocked', data: {} }).text, 'marked task blocked');
  assert.equal(flowEventSummary({ kind: 'custom_kind', data: {} }).text, 'recorded custom kind');
  const nested = flowEventSummary({ kind: 'progress', data: { result: { tests: 8, ok: true }, files: ['a', 'b'] } });
  for (const [, text] of nested.details) assert.doesNotMatch(text, /[{}"]/);
  assert.deepEqual(nested.details.map(([, text]) => text), ['tests 8; ok yes', 'a, b']);
  assert.equal(flowValueText(null), '—');
});

import { flowEventSummary, flowPostLabel, flowStageSteps, flowTrackedWork, flowValueText } from './board-ui.mjs';

test('a resumed subagent shows one completed run and one interrupted run', () => {
  const starts = [
    { event: 'SubagentStart', agent_id: 'a1', timestamp_ms: 1000 },
    { event: 'SubagentStart', agent_id: 'a1', timestamp_ms: 5000 }
  ];
  const stops = [{ event: 'SubagentStop', agent_id: 'a1', timestamp_ms: 7000 }];
  const runs = pairSubagentRuns(starts, stops);
  assert.equal(runs.length, 2);
  assert.equal(runs[0].start.timestamp_ms, 1000);
  assert.equal(runs[0].stop, null);
  assert.equal(runs[0].interrupted, true);
  assert.equal(runs[1].stop.timestamp_ms, 7000);
  assert.equal(runs[1].duration, 2000);
  assert.equal(runs[1].interrupted, false);
  assert.equal(runs.filter(run => !run.stop && !run.interrupted).length, 0);
});

test('subagent runs pair by agent id regardless of event order, and an open run stays running', () => {
  const starts = [
    { agent_id: 'b', timestamp_ms: 2000 },
    { agent_id: 'a', timestamp_ms: 1000 },
    { agent_id: 'c', timestamp_ms: 3000 }
  ];
  const stops = [{ agent_id: 'a', timestamp_ms: 4000 }, { agent_id: 'b', timestamp_ms: 2500 }];
  const runs = pairSubagentRuns(starts, stops);
  assert.deepEqual(runs.map(run => [run.start.agent_id, run.duration, run.interrupted]), [['a', 3000, false], ['b', 500, false], ['c', null, false]]);
  assert.equal(runs.filter(run => !run.stop && !run.interrupted).length, 1);
});

test('subagent runs without ids pair each stop with the most recent earlier open start', () => {
  const starts = [{ timestamp_ms: 1000 }, { timestamp_ms: 2000 }];
  const stops = [{ timestamp_ms: 2500 }, { timestamp_ms: 3000 }, { timestamp_ms: 500 }];
  const runs = pairSubagentRuns(starts, stops);
  assert.deepEqual(runs.map(run => run.duration), [2000, 500]);
  assert.ok(runs.every(run => !run.interrupted));
  assert.deepEqual(pairSubagentRuns([{ timestamp_ms: 1000 }, { timestamp_ms: 2000 }], []).map(run => run.interrupted), [false, false]);
});

import { pairSubagentRuns } from './board-ui.mjs';

test('a stop with no id prefers a start with no id, and a repeated stop never closes an interrupted run', () => {
  const mixed = pairSubagentRuns(
    [{ agent_id: 'a', timestamp_ms: 1 }, { agent_id: 'b', timestamp_ms: 2 }, { timestamp_ms: 3 }],
    [{ timestamp_ms: 4 }, { agent_id: 'b', timestamp_ms: 5 }]
  );
  assert.deepEqual(mixed.map(run => run.duration), [null, 3, 1]);
  const repeated = pairSubagentRuns(
    [{ agent_id: 'a', timestamp_ms: 1 }, { agent_id: 'a', timestamp_ms: 5 }],
    [{ agent_id: 'a', timestamp_ms: 7 }, { agent_id: 'a', timestamp_ms: 8 }]
  );
  assert.deepEqual(repeated.map(run => [run.duration, run.interrupted]), [[null, true], [2, false]]);
});

test('subagent token usage goes to the run that was going on, not every run of the agent', () => {
  const runs = pairSubagentRuns(
    [{ agent_id: 'a', timestamp_ms: 1000 }, { agent_id: 'a', timestamp_ms: 5000 }, { agent_id: 'b', timestamp_ms: 2000 }],
    [{ agent_id: 'a', timestamp_ms: 7000 }]
  );
  const usage = (agent_id, timestamp_ms, input_tokens) => ({ event: 'TurnUsage', agent_id, timestamp_ms, input_tokens, output_tokens: 1 });
  const orphan = attributeSubagentUsage(runs, [
    usage('a', 500, 1), usage('a', 2000, 10), usage('a', 6000, 100), usage('b', 3000, 7), usage('c', 3000, 3)
  ]);
  const byStart = Object.fromEntries(runs.map(run => [`${run.start.agent_id}@${run.start.timestamp_ms}`, run.usage?.fresh]));
  assert.deepEqual(byStart, { 'a@1000': 11, 'b@2000': 7, 'a@5000': 100 });
  assert.deepEqual([...orphan.keys()], ['c']);
  assert.equal(orphan.get('c').fresh, 3);
});

test('an open subagent run ends with its session, goes quiet after the idle limit, and runs on with recent activity', () => {
  const hour = 60 * 60 * 1000;
  const now = 10 * hour;
  const start = (agent_id, session_key, timestamp_ms) => ({ event: 'SubagentStart', agent_id, session_key, timestamp_ms });
  const starts = [start('ended', 's1', now - hour), start('quiet', 's2', now - hour), start('live', 's3', now - hour), start('done', 's4', now - hour)];
  const events = [
    ...starts,
    { event: 'SessionEnd', session_key: 's1', timestamp_ms: now - hour / 2 },
    { event: 'PreToolUse', agent_id: 'quiet', session_key: 's2', timestamp_ms: now - 40 * 60 * 1000 },
    { event: 'PostToolUse', agent_id: 'live', session_key: 's3', timestamp_ms: now - 60 * 1000 },
    // An earlier session end does not end a run that starts later.
    { event: 'SessionEnd', session_key: 's3', timestamp_ms: now - 2 * hour }
  ];
  const runs = inferSubagentEnds(pairSubagentRuns(starts, [{ agent_id: 'done', timestamp_ms: now - 1000 }]), events, now);
  assert.deepEqual(Object.fromEntries(runs.map(run => [run.start.agent_id, run.ended ?? null])), { ended: 'session_ended', quiet: 'idle', live: null, done: null });
  assert.equal(runs.find(run => run.start.agent_id === 'live').lastSeen, now - 60 * 1000);
  assert.equal(runs.filter(run => !run.stop && !run.interrupted && !run.ended).length, 1);
});

import { attributeSubagentUsage, inferSubagentEnds } from './board-ui.mjs';

test('a subagent that completed and was later resumed keeps its completed first run', () => {
  const runs = pairSubagentRuns(
    [{ agent_id: 'a', timestamp_ms: 1 }, { agent_id: 'a', timestamp_ms: 10 }],
    [{ agent_id: 'a', timestamp_ms: 5 }, { agent_id: 'a', timestamp_ms: 15 }]
  );
  assert.deepEqual(runs.map(run => [run.duration, run.interrupted]), [[4, false], [5, false]]);
});
