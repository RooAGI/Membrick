export function escapeHtml(value) {
  return String(value).replace(/[&<>\'\"]/g, character => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', "'": '&#39;', '"': '&quot;'
  }[character]));
}

// The selected board page and the latest page can overlap. Keep one version
// per sequence (the later source wins) and return the normal timeline order.
export function mergeBoardPosts(loadedPosts = [], latestPosts = []) {
  const bySequence = new Map();
  for (const post of loadedPosts) bySequence.set(post.sequence, post);
  for (const post of latestPosts) bySequence.set(post.sequence, post);
  return [...bySequence.values()].sort((a, b) => a.sequence - b.sequence);
}

export function parseBoardLinkTarget(target, knownBoardIds = []) {
  const value = String(target || '');
  const marker = ':post:';
  const separator = value.lastIndexOf(marker);
  if (separator > 0) {
    const parsed = Number(value.slice(separator + marker.length));
    return {
      kind: 'post',
      boardId: value.slice(0, separator),
      sequence: Number.isInteger(parsed) && parsed > 0 ? parsed : null
    };
  }
  if (value.startsWith('board:') || knownBoardIds.includes(value)) {
    return { kind: 'board', boardId: value, sequence: null };
  }
  if (value.startsWith('https://')) return { kind: 'external', boardId: null, sequence: null };
  return { kind: 'reference', boardId: null, sequence: null };
}

// Link relations that make a post an update to an earlier post. depends_on and
// extends relate separate tasks and never change a task's status.
export const UPDATE_RELATIONS = new Set(['tracks', 'evidence_for', 'reviews', 'verifies']);

function postTitle(post) {
  if (post.title) return String(post.title);
  return String(post.content || '').split('\n')[0].trim() || `Post #${post.sequence}`;
}

// Same-board post sequences this post links to with one of the given relations.
function linkedSequences(post, boardId, relations) {
  const sequences = [];
  for (const link of Array.isArray(post.links) ? post.links : []) {
    if (!relations(link?.relation)) continue;
    const target = parseBoardLinkTarget(link.target);
    if (target.kind === 'post' && target.boardId === boardId && target.sequence) sequences.push(target.sequence);
  }
  return sequences;
}

// Roll a board's posts up into one entry per task. Structure only: a post that
// links to an earlier post on this board with an update relation joins that
// post's task, following chains back to the root. A post with links to several
// roots updates each of them. A task's status is the status field of the newest
// post in its set. Posts without a post template or a status value are ignored.
export function rollupTasks(posts = [], boardId, groupComponent = {}) {
  const component = groupComponent && typeof groupComponent === 'object' ? groupComponent : {};
  const statusField = typeof component.group_by === 'string' ? component.group_by : 'status';
  const templateIds = Array.isArray(component.post_template_ids) ? component.post_template_ids : null;
  const declared = Array.isArray(component.groups)
    ? component.groups.filter(group => group && typeof group === 'object' && Object.hasOwn(group, 'value'))
    : [];

  const eligible = new Map();
  for (const post of [...posts].sort((a, b) => a.sequence - b.sequence)) {
    if (!post?.post_template_id || (templateIds && !templateIds.includes(post.post_template_id))) continue;
    const status = post.fields && typeof post.fields === 'object' ? post.fields[statusField] : undefined;
    if (status === undefined || status === null || status === '') continue;
    eligible.set(post.sequence, post);
  }

  // Posts arrive in sequence order and only link backwards, so each target's
  // roots are known before the posts that update it.
  const rootsOf = new Map();
  const tasks = new Map();
  for (const [sequence, post] of eligible) {
    const targets = linkedSequences(post, boardId, relation => UPDATE_RELATIONS.has(relation))
      .filter(target => target < sequence && rootsOf.has(target));
    const roots = targets.length ? [...new Set(targets.flatMap(target => rootsOf.get(target)))].sort((a, b) => a - b) : [sequence];
    rootsOf.set(sequence, roots);
    if (!targets.length) tasks.set(sequence, { post, members: [post] });
    else for (const root of roots) tasks.get(root).members.push(post);
  }

  const groups = declared.map(group => ({ value: group.value, label: String(group.label ?? group.value), tasks: [] }));
  const other = { value: null, label: 'Other', tasks: [] };
  const entries = [];
  for (const [sequence, { post, members }] of tasks) {
    const latest = members[members.length - 1];
    const status = latest.fields[statusField];
    const assigneePost = [...members].reverse().find(member => typeof member.fields.assignee === 'string' && member.fields.assignee.trim());
    const dependsOn = new Set();
    for (const member of members) {
      for (const target of linkedSequences(member, boardId, relation => relation === 'depends_on')) {
        for (const root of rootsOf.get(target) || [target]) if (root !== sequence) dependsOn.add(root);
      }
    }
    const group = groups.find(item => Object.is(item.value, status)) || null;
    const entry = {
      sequence,
      postId: post.post_id || null,
      title: postTitle(post),
      status,
      statusLabel: group ? group.label : String(status),
      group: group ? group.value : null,
      assignee: assigneePost ? assigneePost.fields.assignee : null,
      updates: members.slice(1).map(member => member.sequence),
      latest: { sequence: latest.sequence, title: postTitle(latest), createdAt: latest.created_at || null, status },
      dependsOn: [...dependsOn].sort((a, b) => a - b),
      post
    };
    entries.push(entry);
    (group || other).tasks.push(entry);
  }

  return {
    statusField,
    tasks: entries,
    groups: groups.map(group => ({ ...group, count: group.tasks.length })),
    other: { ...other, count: other.tasks.length },
    rootsOf,
    total: entries.length
  };
}

// Tasks ordered by their most recent activity, newest first: the sequence of
// each task's latest post, then the task's own sequence, both descending.
// Returns a new array and leaves the input untouched.
export function tasksByRecentActivity(tasks = []) {
  return tasks.slice().sort((a, b) => (b.latest?.sequence ?? b.sequence) - (a.latest?.sequence ?? a.sequence) || b.sequence - a.sequence);
}

/* ── Subagent run helpers (pure, used by the Subagent activity panel) ── */

// Pairs SubagentStart and SubagentStop hook events into runs, oldest first.
// Each stop closes the most recent earlier open start with the same agent_id;
// without a match it falls back to starts with no id, and a stop with no id
// tries starts with no id before any other open start. A start that the same
// agent_id started again before the stop was superseded and never takes it.
// A run left without a stop whose agent_id starts again later was interrupted
// (for example killed by an API error and then resumed), so it is not running.
// Each run is { start, stop, duration, interrupted }.
export function pairSubagentRuns(starts = [], stops = []) {
  const at = event => Number(event.timestamp_ms || 0);
  const runs = starts.slice().sort((a, b) => at(a) - at(b))
    .map(start => ({ start, stop: null, duration: null, interrupted: false }));
  const restartedBy = (run, time) => Boolean(run.start.agent_id) && runs.some(later => later !== run
    && later.start.agent_id === run.start.agent_id && at(later.start) > at(run.start) && at(later.start) <= time);
  for (const stop of stops.slice().sort((a, b) => at(a) - at(b))) {
    const open = runs.filter(run => !run.stop && at(run.start) <= at(stop) && !restartedBy(run, at(stop)));
    const noId = open.filter(run => !run.start.agent_id);
    const sameId = stop.agent_id ? open.filter(run => run.start.agent_id === stop.agent_id) : [];
    const candidates = stop.agent_id ? (sameId.length ? sameId : noId) : (noId.length ? noId : open);
    const run = candidates[candidates.length - 1];
    if (!run) continue;
    run.stop = stop;
    run.duration = Math.max(0, at(stop) - at(run.start));
  }
  for (const run of runs) run.interrupted = !run.stop && restartedBy(run, Infinity);
  return runs;
}

const subagentKey = event => event.agent_id || event.agent_type || null;

// Adds each usage event's tokens to the run of the same agent that was going
// on when it happened: the latest run that started at or before it, or the
// first run when it came earlier. Sets run.usage on runs that got any, and
// returns usage for agents with no run, keyed by agent_id or agent_type.
export function attributeSubagentUsage(runs = [], usageEvents = []) {
  const at = event => Number(event.timestamp_ms || 0);
  const byKey = new Map();
  for (const run of runs) {
    const key = subagentKey(run.start);
    if (!key) continue;
    if (!byKey.has(key)) byKey.set(key, []);
    byKey.get(key).push(run);
  }
  byKey.forEach(list => list.sort((a, b) => at(a.start) - at(b.start)));
  const orphan = new Map();
  for (const event of usageEvents) {
    const key = subagentKey(event);
    if (!key) continue;
    const list = byKey.get(key);
    let target;
    if (list) {
      target = list.filter(run => at(run.start) <= at(event)).at(-1) || list[0];
      target.usage ||= { fresh: 0, cacheWrite: 0, cacheRead: 0, output: 0 };
    } else if (!orphan.has(key)) {
      orphan.set(key, { fresh: 0, cacheWrite: 0, cacheRead: 0, output: 0 });
    }
    const usage = target ? target.usage : orphan.get(key);
    usage.fresh += Number(event.input_tokens || 0);
    usage.cacheWrite += Number(event.cache_creation_input_tokens || 0);
    usage.cacheRead += Number(event.cache_read_input_tokens || 0);
    usage.output += Number(event.output_tokens || 0);
  }
  return orphan;
}

export const SUBAGENT_IDLE_MS = 30 * 60 * 1000;

// Infers an end for runs that are open and not interrupted, since a provider
// can die without sending SubagentStop. Sets run.ended to 'session_ended' when
// the run's session sent SessionEnd after it started (the parent's Stop is not
// used, because background subagents outlive the turn), or to 'idle' when no
// event carrying its agent_id arrived within idleMs of now. Sets run.lastSeen.
export function inferSubagentEnds(runs = [], events = [], now = Date.now(), idleMs = SUBAGENT_IDLE_MS) {
  const at = event => Number(event.timestamp_ms || 0);
  for (const run of runs) {
    if (run.stop || run.interrupted) continue;
    const id = run.start.agent_id;
    run.lastSeen = events
      .filter(event => id && event.agent_id === id && at(event) >= at(run.start))
      .reduce((latest, event) => Math.max(latest, at(event)), at(run.start));
    const sessionEnded = run.start.session_key && events.some(event => event.event === 'SessionEnd'
      && event.session_key === run.start.session_key && at(event) >= at(run.start));
    run.ended = sessionEnded ? 'session_ended' : now - run.lastSeen > idleMs ? 'idle' : null;
  }
  return runs;
}

/* ── Flow view helpers (pure, used by the Flows view in app.js) ── */

// Stage stepper states from the pinned template's stage list. Stages before the
// current one are done, as are stages a stage_completed event names. A finished
// flow marks its current stage done too. An unknown current stage is appended.
export function flowStageSteps(stages = [], currentStage = null, flowState = null, events = []) {
  const list = (Array.isArray(stages) ? stages : []).filter(stage => stage && typeof stage === 'object' && stage.id != null);
  const completed = new Set();
  for (const event of Array.isArray(events) ? events : []) {
    if (event?.kind === 'stage_completed' && event.data && event.data.stage != null) completed.add(String(event.data.stage));
  }
  const finished = flowState === 'completed';
  let index = list.findIndex(stage => String(stage.id) === String(currentStage));
  const steps = list.map(stage => ({ id: String(stage.id), name: String(stage.name || stage.id), description: stage.description ? String(stage.description) : '' }));
  if (index < 0 && currentStage != null && currentStage !== '') {
    steps.push({ id: String(currentStage), name: String(currentStage), description: '' });
    index = steps.length - 1;
  }
  return steps.map((step, position) => {
    let stepState = 'upcoming';
    if (index >= 0 && position < index) stepState = 'done';
    else if (position === index) stepState = finished ? 'done' : 'current';
    if (stepState === 'upcoming' && completed.has(step.id)) stepState = 'done';
    return { ...step, state: stepState };
  });
}

// "Task #35 · Production resident path" for a known post, "Post #35" otherwise.
export function flowPostLabel(post, sequence) {
  const kind = post?.post_template_id ? String(post.post_template_id).replaceAll('_', ' ') : 'post';
  const prefix = `${kind.charAt(0).toUpperCase()}${kind.slice(1)} #${sequence}`;
  return post ? `${prefix} · ${postTitle(post)}` : prefix;
}

// The root tasks a flow tracks: every same-board post reference resolved
// through rollup.rootsOf to its task. References that do not resolve to a task
// (other boards, board references, posts without a status) are returned as
// otherReferences. References are deduped by target, keeping every relation.
// groups and other mirror rollup.groups and rollup.other (same order, same
// labels, zero counts kept) but hold only the tracked tasks, so the Flow tiles
// use exactly the board's declared groups.
export function flowTrackedWork(references = [], rollup, boardId) {
  const byId = new Map();
  for (const reference of Array.isArray(references) ? references : []) {
    if (!reference || !reference.id) continue;
    const id = String(reference.id);
    const entry = byId.get(id) || { id, kind: reference.kind || null, relations: [] };
    if (reference.relation && !entry.relations.includes(reference.relation)) entry.relations.push(reference.relation);
    byId.set(id, entry);
  }
  const rootsOf = rollup?.rootsOf instanceof Map ? rollup.rootsOf : new Map();
  const viaRoot = new Map();
  const otherReferences = [];
  for (const reference of byId.values()) {
    const target = parseBoardLinkTarget(reference.id);
    const roots = target.kind === 'post' && target.boardId === boardId && target.sequence ? rootsOf.get(target.sequence) : null;
    if (!roots || !roots.length) { otherReferences.push({ ...reference, target }); continue; }
    for (const root of roots) {
      const item = viaRoot.get(root) || { direct: false, references: [] };
      if (root === target.sequence) item.direct = true;
      item.references.push(target.sequence);
      viaRoot.set(root, item);
    }
  }
  const tasks = (rollup?.tasks || [])
    .filter(task => viaRoot.has(task.sequence))
    .map(task => ({ ...task, direct: viaRoot.get(task.sequence).direct, references: viaRoot.get(task.sequence).references }));
  const subset = group => {
    const members = new Set((group.tasks || []).map(item => item.sequence));
    const groupTasks = tasks.filter(task => members.has(task.sequence));
    return { value: group.value, label: group.label, tasks: groupTasks, count: groupTasks.length };
  };
  const groups = (rollup?.groups || []).map(subset);
  const other = subset(rollup?.other || { value: null, label: 'Other', tasks: [] });
  return { tasks, otherReferences, groups, other, total: tasks.length };
}

// Plain text for an event data value. Nested objects become "key value" pairs,
// never JSON.
export function flowValueText(value) {
  if (value == null || value === '') return '—';
  if (typeof value === 'boolean') return value ? 'yes' : 'no';
  if (Array.isArray(value)) return value.map(flowValueText).join(', ') || '—';
  if (typeof value === 'object') {
    const entries = Object.entries(value);
    return entries.length ? entries.map(([key, item]) => `${key.replaceAll('_', ' ')} ${flowValueText(item)}`).join('; ') : '—';
  }
  return String(value);
}

const FLOW_EVENT_VERBS = {
  flow_completed: 'completed the flow',
  flow_cancelled: 'cancelled the flow',
  flow_failed: 'marked the flow failed',
  coordinator_claimed: 'claimed coordination',
  coordinator_released: 'released coordination',
  task_created: 'created task{task}',
  task_started: 'started task{task}',
  progress: 'reported progress on task{task}',
  blocked: 'marked task{task} blocked',
  ready_for_review: 'sent task{task} for review',
  review_approved: 'approved task{task}',
  review_changes_requested: 'requested changes on task{task}',
  task_cancelled: 'cancelled task{task}',
  action_consumed: 'consumed an action'
};
// Data keys a summary already says, or that are internal bookkeeping.
const FLOW_EVENT_HIDDEN = new Set(['coordinator_fence', 'reference', 'stage', 'template_id', 'template_version', 'template_hash', 'provider']);

// One readable line per Flow event: { text, reference, details }. text follows
// the actor's name ("moved the flow to Independent review"). reference is a
// { id, kind, relation } the caller renders as a titled link, placed after the
// text. details lists the remaining data as [label, plain text, raw value].
export function flowEventSummary(event = {}, stageName = id => id) {
  const kind = String(event.kind || 'event');
  const data = event.data && typeof event.data === 'object' && !Array.isArray(event.data) ? event.data : {};
  const task = event.task_id ? ` ${event.task_id}` : '';
  let text;
  let reference = null;
  const hidden = new Set(FLOW_EVENT_HIDDEN);
  if (kind === 'flow_started') {
    text = `started the flow${data.template_id ? ` from ${data.template_id}${data.template_version != null ? ` v${data.template_version}` : ''}` : ''}`;
  } else if (kind === 'stage_changed') {
    text = data.stage != null ? `moved the flow to ${stageName(String(data.stage))}` : 'changed the stage';
  } else if (kind === 'stage_completed') {
    text = data.stage != null ? `completed stage ${stageName(String(data.stage))}` : 'completed a stage';
  } else if (kind === 'coordinator_handed_off') {
    const to = data.to_agent_id || data.agent_id || data.to;
    text = to ? `handed coordination to ${flowValueText(to)}` : 'handed off coordination';
    for (const key of ['to_agent_id', 'agent_id', 'to']) hidden.add(key);
  } else if (kind === 'task_assigned') {
    const to = data.assignee?.agent_id || data.agent_id || data.assignee;
    text = `assigned task${task}${to ? ` to ${flowValueText(to)}` : ''}`;
    for (const key of ['assignee', 'agent_id']) hidden.add(key);
  } else if (kind === 'action_requested') {
    const action = data.action || data.action_kind || data.kind;
    text = `requested action${action ? ` ${flowValueText(action)}` : ''}`;
    for (const key of ['action', 'action_kind', 'kind']) hidden.add(key);
  } else if (kind === 'reference_added') {
    const ref = data.reference && typeof data.reference === 'object' ? data.reference : null;
    text = 'added';
    if (ref?.id) reference = { id: String(ref.id), kind: ref.kind || null, relation: ref.relation || null };
    else text = 'added a reference';
  } else if (FLOW_EVENT_VERBS[kind]) {
    text = FLOW_EVENT_VERBS[kind].replace('{task}', task);
  } else {
    text = `recorded ${kind.replaceAll('_', ' ')}${task}`;
  }
  const details = Object.entries(data)
    .filter(([key]) => !hidden.has(key))
    .map(([key, value]) => [key.replaceAll('_', ' '), flowValueText(value), value]);
  return { text, reference, details };
}
