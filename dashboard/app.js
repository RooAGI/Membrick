import { renderEventDetail, renderToolCall, toolDetail, toolGroup } from './event-detail.js';
import { renderCaptureProgressHtml } from './capture-progress.mjs';
import { renderSearchEvaluationHtml } from './search-evaluation.mjs';
import { flowEventSummary, flowPostLabel, flowStageSteps, flowTrackedWork, mergeBoardPosts as mergeFlowBoardPosts, parseBoardLinkTarget as parseFlowLinkTarget, rollupTasks as rollupFlowBoardTasks } from './board-ui.mjs';
import { attributeSubagentUsage, escapeHtml as esc, inferSubagentEnds, mergeBoardPosts, pairSubagentRuns, parseBoardLinkTarget, SUBAGENT_IDLE_MS, rollupTasks, tasksByRecentActivity } from './board-ui.mjs';

const VIEWS = {
  overview: { title: 'Overview', sub: 'How the memory index is serving agents right now.' },
  index: { title: 'Index', sub: 'Segment topology and document distribution of the primary query index.' },
  providers: { title: 'Providers', sub: 'Per-integration sessions, tool use, and private memory stores.' },
  boards: { title: 'Boards', sub: 'Project structure and updates posted by agents.' },
  flows: { title: 'Flows', sub: 'Provider-neutral workflow stages, board posts, and event history.' },
  reviews: { title: 'Search reviews', sub: 'Submitted feedback and offline ranking evaluation.' }
};

const state = { view: 'overview', status: null, series: [], provider: null, tool: null, hover: null, reviewOffset: 0, reviewTotal: 0, boards: [], selectedBoardId: null, boardSnapshot: null, boardTrail: [], boardGeneration: 0, boardRenderSeq: 0, boardTick: 0, linkedBoards: new Map(), flows: [], selectedFlowId: null, flowSnapshot: null };

// Browser storage can be blocked or throw (private windows, disabled site
// data). Every read and write goes through these so the page still works.
function storageGet(key) {
  try { return window.localStorage.getItem(key); } catch { return null; }
}
function storageSet(key, value) {
  try { window.localStorage.setItem(key, value); } catch { /* storage unavailable */ }
}

const tokenInput = document.querySelector('#token');
tokenInput.value = storageGet('membrick-dashboard-token') || '';
tokenInput.addEventListener('change', () => storageSet('membrick-dashboard-token', tokenInput.value));
document.querySelector('#refresh').addEventListener('click', refresh);

async function api(path) {
  const token = tokenInput.value.trim().replace(/^Bearer\s+/i, '');
  const response = await fetch(path, { cache: 'no-store', headers: token ? { Authorization: `Bearer ${token}` } : {} });
  if (!response.ok) throw new Error(`${response.status} ${response.statusText}`);
  return response.json();
}

const fmt = value => Number(value || 0).toLocaleString();
const clock = value => value ? new Date(value).toLocaleTimeString() : '—';
const pct = value => `${(Number(value || 0) * 100).toFixed(1)}%`;
const seconds = ms => ms ? `${(ms / 1000).toFixed(1)}s` : '—';
const cssVar = name => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
const stillness = window.matchMedia('(prefers-reduced-motion: reduce)');

// The Boards and Flows list panels collapse to a narrow rail. The choice is a
// class on the static layout element, so detail re-renders never reset it.
// CSS applies the rail only while the list sits beside the detail.
document.querySelectorAll('[data-rail-toggle]').forEach(button => {
  const page = button.dataset.railToggle;
  const layout = button.closest('.boards-layout, .flows-layout');
  const noun = button.dataset.railNoun || 'list';
  const key = `membrick-dashboard-rail-${page}`;
  const apply = collapsed => {
    layout.classList.toggle('is-rail', collapsed);
    button.setAttribute('aria-expanded', String(!collapsed));
    button.setAttribute('aria-label', collapsed ? `Expand ${noun}` : `Collapse ${noun}`);
    button.title = collapsed ? `Expand ${noun}` : `Collapse ${noun}`;
  };
  apply(storageGet(key) === 'collapsed');
  button.addEventListener('click', () => {
    const collapsed = !layout.classList.contains('is-rail');
    apply(collapsed);
    storageSet(key, collapsed ? 'collapsed' : 'expanded');
  });
});

// Work on templated boards shows as Columns (the default) or a List. Phone
// widths always use the List.
const WORK_MODE_KEY = 'membrick-dashboard-work-view';
const phoneWidth = window.matchMedia('(max-width: 699.98px)');
let workMode = storageGet(WORK_MODE_KEY) === 'list' ? 'list' : 'columns';
const effectiveWorkMode = () => phoneWidth.matches ? 'list' : workMode;
function setWorkMode(mode) {
  if (mode !== 'list' && mode !== 'columns') return Promise.resolve();
  workMode = mode;
  storageSet(WORK_MODE_KEY, mode);
  return state.boardSnapshot ? renderBoardDetail(state.boardSnapshot) : Promise.resolve();
}

// Sequences of the posts that belong to a task in the main Work panel.
let boardTaskPosts = new Set();
// In Columns, the task whose details show under the columns, by Work panel
// scope. It outlives the DOM, so live re-renders keep it open and selected.
const workOpenTask = new Map();
let workOpenBoardId = null;
// The task view each Work panel was last rendered from, by scope.
const workViews = new Map();
// Open or closed state of each disclosure on the selected board, by detail ID.
const boardDetailState = new Map();
phoneWidth.addEventListener('change', () => {
  if (state.boardSnapshot && workMode === 'columns') renderBoardDetail(state.boardSnapshot);
});

// Animate a figure between readings. `render` receives the interpolated numbers,
// so composite readouts (p50 / p95) move as one.
function tween(element, next, render) {
  if (!element) return;
  const previous = JSON.parse(element.dataset.figures || 'null');
  element.dataset.figures = JSON.stringify(next);
  if (stillness.matches || !previous || previous.length !== next.length) {
    element.textContent = render(next);
    return;
  }
  cancelAnimationFrame(Number(element.dataset.frame || 0));
  const started = performance.now();
  const step = now => {
    const progress = Math.min(1, (now - started) / 520);
    const eased = 1 - Math.pow(1 - progress, 3);
    element.textContent = render(next.map((value, index) => previous[index] + (value - previous[index]) * eased));
    if (progress < 1) element.dataset.frame = requestAnimationFrame(step);
  };
  element.dataset.frame = requestAnimationFrame(step);
}

// Second half of the window against the first: the direction the measure is moving.
function trend(values) {
  if (values.length < 6) return null;
  const mean = list => list.reduce((sum, value) => sum + value, 0) / (list.length || 1);
  const half = Math.floor(values.length / 2);
  const before = mean(values.slice(0, half));
  const after = mean(values.slice(half));
  if (!before && !after) return null;
  const change = before ? (after - before) / before : 1;
  return { change, direction: Math.abs(change) < 0.02 ? 'flat' : change > 0 ? 'up' : 'down' };
}

// `polarity` says what a rise means: 'lower-better' for latency and errors,
// 'neutral' for throughput, where neither direction is good news by itself.
function renderDelta(element, values, polarity) {
  if (!element) return;
  const moved = trend(values);
  if (!moved) { element.textContent = ''; element.className = 'delta'; return; }
  const glyph = moved.direction === 'flat' ? '→' : moved.direction === 'up' ? '↑' : '↓';
  const tone = polarity === 'neutral' || moved.direction === 'flat'
    ? ''
    : (moved.direction === 'up') === (polarity === 'lower-better') ? ' bad' : ' good';
  element.className = `delta${tone}`;
  element.textContent = `${glyph} ${Math.abs(moved.change * 100).toFixed(0)}%`;
  element.title = `${moved.direction === 'flat' ? 'Steady' : moved.direction === 'up' ? 'Up' : 'Down'} versus the first half of the window`;
}

/* ── view routing ──────────────────────────────────────────────── */

// A route is a view name, optionally followed by an encoded item ID, such as
// `boards/<board_id>`, so a specific board can be linked to directly.
function showView(route, loadData = true) {
  const [name, ...rest] = String(route || '').split('/');
  let view = name;
  const itemId = rest.length ? decodeURIComponent(rest.join('/')) : null;
  if (!VIEWS[view]) view = 'overview';
  state.view = view;
  document.querySelectorAll('.view').forEach(section => { section.hidden = section.id !== `view-${view}`; });
  document.querySelectorAll('.navitem').forEach(item => {
    if (item.dataset.view === view) item.setAttribute('aria-current', 'page');
    else item.removeAttribute('aria-current');
  });
  document.querySelector('#view-title').textContent = VIEWS[view].title;
  document.querySelector('#view-sub').textContent = VIEWS[view].sub;
  if (location.hash.slice(1).split('/')[0] !== view) history.replaceState(null, '', `#${view}`);
  document.querySelector('.viewport').scrollTop = 0;
  if (view === 'overview') drawSparks();
  if (view === 'reviews') loadSearchReviews(state.reviewOffset);
  if (loadData && view === 'boards') loadBoards(itemId || state.selectedBoardId);
  if (loadData && view === 'flows') loadFlows(itemId || state.selectedFlowId);
  // `providers/<name>` selects a provider by its display name, ignoring case.
  if (view === 'providers' && itemId) {
    const items = state.status?.integrations || [];
    const match = items.find(item => item.provider.toLowerCase() === itemId.toLowerCase());
    state.provider = match ? match.provider : itemId;
    state.tool = null;
    if (items.length) renderProviders(items);
  }
}

document.querySelectorAll('.navitem').forEach(item => item.addEventListener('click', () => showView(item.dataset.view)));
document.querySelectorAll('[data-goto]').forEach(item => item.addEventListener('click', () => showView(item.dataset.goto)));
window.addEventListener('hashchange', () => showView(location.hash.slice(1)));

/* ── submitted search feedback ────────────────────────────────── */

async function loadSearchReviews(offset = 0) {
  const container = document.querySelector('#search-reviews');
  container.innerHTML = '<div class="empty-state">Loading submitted feedback…</div>';
  try {
    const page = await api(`/api/search-reviews?offset=${offset}&page_size=25`);
    state.reviewOffset = page.offset || 0;
    state.reviewTotal = Number(page.total || 0);
    const reviews = page.reviews || [];
    if (page.evaluation_status === 'ready') {
      renderSearchEvaluation(page.evaluation || {}, page.evaluation_generated_at);
    } else {
      document.querySelector('#search-review-evaluation').innerHTML = '<section class="review-evaluation__panel"><p class="eyebrow">Offline ranking evaluation</p><p class="muted">Not run yet. Run the offline feedback evaluator to refresh this summary.</p></section>';
    }
    document.querySelector('#search-reviews-count').textContent = `${fmt(state.reviewTotal)} reviews`;
    document.querySelector('#search-reviews-page').textContent = state.reviewTotal
      ? `${fmt(state.reviewOffset + 1)}–${fmt(state.reviewOffset + reviews.length)} of ${fmt(state.reviewTotal)}`
      : 'No submitted feedback yet';
    document.querySelector('#search-reviews-previous').disabled = state.reviewOffset === 0;
    document.querySelector('#search-reviews-next').disabled = page.next_offset == null;
    document.querySelector('#search-reviews-previous').onclick = () => loadSearchReviews(Math.max(0, state.reviewOffset - 25));
    document.querySelector('#search-reviews-next').onclick = () => loadSearchReviews(page.next_offset);
    container.innerHTML = reviews.length ? reviews.map(renderSearchReview).join('')
      : page.evaluation_status === 'ready'
        ? '<div class="empty-state">No submitted search feedback yet.</div>'
        : '<div class="empty-state">Run the offline feedback evaluator to populate this review page.</div>';
  } catch (error) {
    document.querySelector('#search-reviews-count').textContent = '';
    document.querySelector('#search-reviews-page').textContent = '';
    document.querySelector('#search-review-evaluation').innerHTML = '';
    container.innerHTML = `<div class="empty-state">Could not load feedback: ${esc(error.message)}</div>`;
  }
}

function renderSearchEvaluation(evaluation, generatedAt) {
  document.querySelector('#search-review-evaluation').innerHTML = renderSearchEvaluationHtml(evaluation, generatedAt);
}

function renderSearchReview(review) {
  const judgments = review.document_judgments || [];
  const judgmentRows = judgments.length ? judgments.map(item => `
    <div class="review-doc">
      <span class="mono">#${fmt(item.rank)} · ${esc(item.doc_id)}</span>
      <span class="review-label review-label--${esc(item.relevance)}">${esc(item.relevance)}</span>
      ${item.feedback ? `<p>${esc(item.feedback)}</p>` : ''}
    </div>`).join('') : '<p class="muted">No document judgments stored.</p>';
  const label = review.label || review.status || 'unknown';
  const submitted = review.submitted_at_ms ? new Date(review.submitted_at_ms).toLocaleString() : 'Date unavailable';
  return `<article class="search-review">
    <div class="search-review__head">
      <div><p class="eyebrow">${esc(review.provider || 'Unknown provider')} · ${esc(submitted)}</p><h3>${esc(review.query || 'Search query unavailable')}</h3></div>
      <span class="review-label review-label--${esc(label)}">${esc(label.replaceAll('_', ' '))}</span>
    </div>
    <p class="search-review__feedback">${esc(review.feedback || 'No overall explanation stored.')}</p>
    <details><summary>${fmt(judgments.length)} document judgments · ${esc(review.search_id)}</summary><div class="review-docs">${judgmentRows}</div></details>
  </article>`;
}

/* ── polling ───────────────────────────────────────────────────── */

async function refresh() {
  if (document.hidden) return;
  // Board and Flow polling is independent of the status request so a slow or
  // failing status endpoint does not stop live updates.
  if (state.view === 'boards') refreshBoards();
  if (state.view === 'flows') pollFlows();
  const pill = document.querySelector('#status-pill');
  try {
    const [status, series] = await Promise.all([api('/api/status'), api('/api/timeseries')]);
    state.status = status;
    document.querySelector('#capture-progress').innerHTML = renderCaptureProgressHtml(status.transcript_captures);
    state.series = series.query_series || [];

    const healthy = status.status === 'healthy';
    pill.className = `pill ${healthy ? 'ok' : 'warn'}`;
    pill.querySelector('span').textContent = healthy ? 'Healthy' : 'Index needs attention';

    const query = status.telemetry.query_summary;
    tween(document.querySelector('#rps'), [Number(query.requests_per_second || 0)], ([rate]) => rate.toFixed(2));
    tween(document.querySelector('#latency'), [query.p50_ms, query.p95_ms], ([p50, p95]) => `${p50.toFixed(0)} / ${p95.toFixed(0)}`);
    tween(document.querySelector('#errors'), [Number(query.errors || 0)], ([errors]) => fmt(Math.round(errors)));
    tween(document.querySelector('#empty'), [Number(query.empty_results || 0)], ([empty]) => fmt(Math.round(empty)));
    document.querySelector('#errors-scope').firstChild.textContent = `${pct(query.error_rate)} of queries `;
    document.querySelector('#empty-scope').firstChild.textContent = `${pct(query.empty_result_rate)} of queries `;
    renderDelta(document.querySelector('#rps-delta'), state.series.map(point => Number(point.requests_per_second || 0)), 'neutral');
    renderDelta(document.querySelector('#latency-delta'), state.series.map(point => Number(point.p95_ms || 0)), 'lower-better');
    renderDelta(document.querySelector('#errors-delta'), state.series.map(point => Number(point.errors || 0)), 'lower-better');
    renderDelta(document.querySelector('#empty-delta'), state.series.map(point => Number(point.empty_results || 0)), 'lower-better');
    document.querySelector('#window').textContent = `${status.telemetry.window_seconds}s rolling window`;
    document.querySelector('#last-updated').textContent = `Updated ${new Date().toLocaleTimeString()}`;
    document.body.classList.remove('booting');

    const integrations = status.integrations || [];
    const projectIndex = projectIndexView(status.index, integrations);
    renderIndexBrief(projectIndex);
    renderIndexTopology(projectIndex);
    renderProviders(integrations, projectIndex);
    renderFeed(integrations);
    drawSparks();
  } catch (error) {
    pill.className = 'pill bad';
    pill.querySelector('span').textContent = 'Unavailable';
    document.querySelector('#last-updated').textContent = error.message;
  }
}

/* ── agent-authored boards ────────────────────────────────────── */

const boardCreatedAt = board => Date.parse(board?.created_at || '') || 0;

// Newest boards first, so the default selection is the most recent work.
function sortBoards(boards) {
  return (boards || []).slice().sort((a, b) => boardCreatedAt(b) - boardCreatedAt(a)
    || String(a.title || a.key).localeCompare(String(b.title || b.key)));
}

// Boards that share a title are one group: the newest copy is shown and the
// older copies sit under an "Earlier" toggle. Groups keep the newest-first order.
function boardTitleGroups(boards) {
  const groups = new Map();
  for (const board of boards) {
    const name = String(board.title || board.key || '').trim().toLowerCase();
    if (!groups.has(name)) groups.set(name, { name, boards: [] });
    groups.get(name).boards.push(board);
  }
  return [...groups.values()];
}

const openBoardGroups = new Set();

function boardChoiceHtml(board, earlier = false) {
  const created = earlier && board.created_at ? `<small class="board-choice__key">Created ${esc(absoluteTime(board.created_at))}</small>` : `<small class="board-choice__key">${esc(board.key)}</small>`;
  return `<button class="board-choice${earlier ? ' board-choice--earlier' : ''}" type="button" data-board-id="${esc(board.board_id)}">
        <span class="board-choice__top"><strong>${esc(board.title || board.key)}</strong>${board.type && board.type !== 'general' ? `<span class="board-choice__type">${esc(board.type)}</span>` : ''}${board.board_template ? '<span class="board-choice__type board-choice__type--template">template</span>' : ''}</span>
        ${created}
        <span class="board-choice__meta" data-board-meta="${esc(board.board_id)}">${esc(boardMetaText(board.board_id))}</span>
      </button>`;
}

// An older copy that is selected (for example through a link) is revealed.
function revealSelectedBoardChoice() {
  document.querySelectorAll('#boards-list .board-choice').forEach(button => button.classList.toggle('is-selected', button.dataset.boardId === state.selectedBoardId));
  const selected = document.querySelector('#boards-list .board-choice.is-selected');
  const earlier = selected?.closest('details[data-board-group]');
  if (earlier && !earlier.open) earlier.open = true;
}

// The list is redrawn only when its visible content changes, so polling does
// not disturb the selection or the sidebar scroll.
function renderBoardList() {
  const list = document.querySelector('#boards-list');
  const signature = JSON.stringify(state.boards.map(board => [board.board_id, board.title, board.key, board.type, board.created_at, Boolean(board.board_template)]));
  const groups = boardTitleGroups(state.boards);
  document.querySelector('#boards-count').textContent = `${fmt(groups.length)} boards${groups.length !== state.boards.length ? ` · ${fmt(state.boards.length - groups.length)} earlier` : ''}`;
  if (list.dataset.signature !== signature) {
    list.dataset.signature = signature;
    list.innerHTML = state.boards.length ? groups.map(group => {
      const [latest, ...older] = group.boards;
      if (!older.length) return boardChoiceHtml(latest);
      return `<div class="board-choice-group">${boardChoiceHtml(latest)}
        <details class="board-earlier" data-board-group="${esc(group.name)}"${openBoardGroups.has(group.name) ? ' open' : ''}>
          <summary>Earlier · ${fmt(older.length)}</summary>
          ${older.map(board => boardChoiceHtml(board, true)).join('')}
        </details></div>`;
    }).join('') : '<div class="empty-state">No boards yet. Agents can open a project board and link planning, task, and milestone boards.</div>';
    list.querySelectorAll('[data-board-id]').forEach(button => button.addEventListener('click', () => {
      state.boardTrail = [];
      selectBoard(button.dataset.boardId);
    }));
    list.querySelectorAll('details[data-board-group]').forEach(item => item.addEventListener('toggle', () => {
      if (item.open) openBoardGroups.add(item.dataset.boardGroup); else openBoardGroups.delete(item.dataset.boardGroup);
    }));
  }
  revealSelectedBoardChoice();
  // Summaries load a few boards at a time and refresh at most once a minute.
  const queue = state.boards.slice(0, 40).map(board => board.board_id);
  const worker = async () => { while (queue.length) await loadBoardMeta(queue.shift()); };
  Promise.all([worker(), worker(), worker()]);
}

async function loadBoards(preferredId = state.selectedBoardId) {
  const list = document.querySelector('#boards-list');
  list.dataset.signature = '';
  list.innerHTML = '<div class="empty-state">Loading boards…</div>';
  try {
    const result = await api('/api/boards');
    state.boards = sortBoards(result.boards);
    const next = state.boards.some(board => board.board_id === preferredId)
      ? preferredId
      : state.boards[0]?.board_id;
    state.selectedBoardId = next || null;
    renderBoardList();
    if (next) await selectBoard(next);
    else document.querySelector('#board-detail').innerHTML = '<div class="empty-state">No agent-authored boards in this workspace.</div>';
  } catch (error) {
    document.querySelector('#boards-count').textContent = '';
    list.innerHTML = `<div class="empty-state">Could not load boards: ${esc(error.message)}</div>`;
  }
}

function lastPostSequence(posts) {
  return posts.reduce((max, post) => Math.max(max, Number(post.sequence) || 0), 0);
}

async function selectBoard(boardId, postSequence = null) {
  state.selectedBoardId = boardId;
  if (state.view === 'boards') history.replaceState(null, '', `#boards/${encodeURIComponent(boardId)}`);
  const generation = ++state.boardGeneration;
  revealSelectedBoardChoice();
  document.querySelector('#board-detail').innerHTML = '<div class="empty-state">Loading board…</div>';
  state.boardSnapshot = null;
  await loadBoardPosts(boardId, null, false);
  let snapshot = state.boardSnapshot;
  if (snapshot?.latestSequence > lastPostSequence(snapshot.posts)) {
    if (await loadLatestBoardPosts(boardId)) await renderBoardDetail(snapshot);
  }
  await loadAllBoardPosts(boardId, generation);
  // Template sections summarize posts from the first page onward, so a linked
  // post is reached by paging forward rather than loading a partial window.
  // The API pages in sequence order, so do not impose an arbitrary page cap.
  while (postSequence != null) {
    snapshot = state.boardSnapshot;
    if (state.boardGeneration !== generation || !snapshot || lastPostSequence(snapshot.posts) >= postSequence || snapshot.latestPosts?.some(post => post.sequence === postSequence)) break;
    if (snapshot.latestSequence != null && postSequence > snapshot.latestSequence) break;
    if (!snapshot.hasMore || !Number.isSafeInteger(snapshot.nextAfter) || snapshot.nextAfter <= lastPostSequence(snapshot.posts)) break;
    document.querySelector('#board-detail').innerHTML = `<div class="empty-state">Loading post ${fmt(postSequence)} · reached post ${fmt(lastPostSequence(snapshot.posts))}…</div>`;
    if (!await loadBoardPosts(boardId, snapshot.nextAfter, true, false)) break;
  }
  if (postSequence != null && state.boardGeneration === generation) {
    await renderBoardDetail(state.boardSnapshot);
    if (!await focusBoardPost(postSequence)) {
      const notice = document.createElement('p');
      notice.className = 'board-link-error';
      notice.textContent = `Post ${postSequence} was not found on this board.`;
      document.querySelector('#board-detail')?.prepend(notice);
    }
  }
}

// A post that belongs to a task opens in the current Work mode: under the
// columns in Columns, or in its row in the List. The mode never changes.
// Resolves to whether the post was found.
async function focusBoardPost(sequence) {
  if (boardTaskPosts.has(sequence) && document.querySelector('#board-detail .board-work--columns[data-work-scope="main"]')) {
    const root = workViews.get('main')?.rollup.rootsOf.get(sequence)?.[0];
    if (root != null) openWorkTask('main', root, { reveal: false });
  }
  const post = document.querySelector(`#post-${sequence}`);
  if (!post) return false;
  // Open every enclosing disclosure (for example a settled group, then the task row).
  if (post.tagName === 'DETAILS') post.open = true;
  for (let parent = post.parentElement?.closest('details'); parent; parent = parent.parentElement?.closest('details')) parent.open = true;
  expandWorkGroupOf(post);
  post.scrollIntoView({ behavior: stillness.matches ? 'auto' : 'smooth', block: 'center' });
  post.classList.add('is-focused');
  setTimeout(() => post.classList.remove('is-focused'), 2400);
  return true;
}

// Expand a collapsed work group that holds the element, and remember it.
function expandWorkGroupOf(element) {
  const group = element.closest('[data-work-group].is-collapsed');
  if (!group) return;
  const boardId = document.querySelector('#board-detail')?.dataset.boardId;
  if (boardId) setWorkGroupCollapsed(boardId, group.dataset.workGroup, false);
  applyWorkGroupCollapsed(group, false);
}

// In Columns, a task's details open in the area under the columns of the same
// Work panel. The mode and the page stay where they are. With `reveal`, the
// page scrolls only when the area's head is below the visible part, and then
// only far enough to show it. With `keyboard`, focus moves to the heading.
function openWorkTask(scope, sequence, { reveal = true, keyboard = false } = {}) {
  const panel = document.querySelector(`#board-detail .board-work--columns[data-work-scope="${CSS.escape(scope)}"]`);
  const area = panel?.querySelector(':scope > [data-task-detail-area]');
  const view = workViews.get(scope);
  const task = view?.rollup.tasks.find(item => item.sequence === sequence);
  if (!area || !task) return false;
  workOpenTask.set(scope, sequence);
  area.innerHTML = taskDetailHtml(task, view, scope);
  area.hidden = false;
  bindBoardLinkButtons(area);
  bindPostFolds(area);
  markSelectedCard(panel, sequence);
  if (reveal) revealWorkDetail(area);
  if (keyboard) area.querySelector('[data-task-detail-title]')?.focus({ preventScroll: true });
  return true;
}

function closeWorkTask(scope, { restoreFocus = false } = {}) {
  const sequence = workOpenTask.get(scope);
  workOpenTask.delete(scope);
  const panel = document.querySelector(`#board-detail .board-work--columns[data-work-scope="${CSS.escape(scope)}"]`);
  const area = panel?.querySelector(':scope > [data-task-detail-area]');
  if (!area) return;
  area.hidden = true;
  area.innerHTML = '';
  markSelectedCard(panel, null);
  if (restoreFocus && sequence != null) panel.querySelector(`[data-task-open="${CSS.escape(String(sequence))}"]`)?.focus({ preventScroll: true });
}

function markSelectedCard(panel, sequence) {
  panel.querySelectorAll('[data-task-open]').forEach(card => {
    const selected = Number(card.dataset.taskOpen) === sequence;
    card.classList.toggle('is-selected', selected);
    card.setAttribute('aria-expanded', String(selected));
  });
}

// Scroll the least amount that brings the detail head into view. Nothing
// moves when it is already visible.
// The page may scroll in .viewport or in the document, so the browser picks
// the scroller: block "nearest" moves only as far as the head needs.
function revealWorkDetail(area) {
  const head = area.querySelector('.board-task-detail__head') || area;
  const rect = head.getBoundingClientRect();
  const viewport = document.querySelector('.viewport');
  const bottom = Math.min(window.innerHeight, viewport && viewport.scrollHeight > viewport.clientHeight ? viewport.getBoundingClientRect().bottom : Infinity);
  if (rect.bottom <= bottom && rect.top >= 0) return;
  head.scrollIntoView({ block: 'nearest', behavior: stillness.matches ? 'auto' : 'smooth' });
}

// Escape closes an open task detail: the one holding focus, else every one.
document.addEventListener('keydown', event => {
  if (event.key !== 'Escape' || event.defaultPrevented || !workOpenTask.size || state.view !== 'boards' || !palette.hidden) return;
  const focused = document.activeElement?.closest?.('.board-work--columns[data-work-scope]');
  const scopes = focused && workOpenTask.has(focused.dataset.workScope) ? [focused.dataset.workScope] : [...workOpenTask.keys()];
  for (const scope of scopes) closeWorkTask(scope, { restoreFocus: Boolean(focused) });
});

async function loadBoardPosts(boardId, afterSequence, append, render = true) {
  const generation = state.boardGeneration;
  const params = new URLSearchParams({ limit: '100' });
  if (afterSequence != null) params.set('after_sequence', String(afterSequence));
  try {
    const result = await api(`/api/boards/${encodeURIComponent(boardId)}?${params}`);
    if (state.selectedBoardId !== boardId || state.boardGeneration !== generation) return;
    if (append && state.boardSnapshot) {
      const known = lastPostSequence(state.boardSnapshot.posts);
      state.boardSnapshot.posts.push(...(result.posts || []).filter(post => post.sequence > known));
      state.boardSnapshot.board = result.board || state.boardSnapshot.board;
    } else state.boardSnapshot = { board: result.board, posts: result.posts || [], newPosts: 0 };
    state.boardSnapshot.hasMore = result.has_more;
    state.boardSnapshot.nextAfter = result.next_after_sequence;
    state.boardSnapshot.latestSequence = result.latest_sequence ?? state.boardSnapshot.latestSequence;
    state.boardSnapshot.checkedAt = Date.now();
    if (state.boardSnapshot.latestSequence != null && lastPostSequence(state.boardSnapshot.posts) >= state.boardSnapshot.latestSequence) state.boardSnapshot.newPosts = 0;
    if (render) await renderBoardDetail(state.boardSnapshot);
    return true;
  } catch (error) {
    if (state.selectedBoardId === boardId && state.boardGeneration === generation) document.querySelector('#board-detail').innerHTML = `<div class="empty-state">Could not load board: ${esc(error.message)}</div>`;
    return false;
  }
}

async function loadLatestBoardPosts(boardId) {
  const generation = state.boardGeneration;
  const snapshot = state.boardSnapshot;
  if (!snapshot || snapshot.board?.board_id !== boardId) return false;
  try {
    const result = await api(`/api/boards/${encodeURIComponent(boardId)}?limit=100&latest=true`);
    if (state.selectedBoardId !== boardId || state.boardGeneration !== generation || state.boardSnapshot !== snapshot) return false;
    snapshot.latestPosts = result.posts || [];
    snapshot.latestSequence = result.latest_sequence ?? lastPostSequence(snapshot.latestPosts);
    snapshot.latestHasMore = Boolean(result.has_more);
    return true;
  } catch {
    return false;
  }
}

// Page forward through the whole board (after_sequence while has_more) so the
// task rollup, tiles and Work counts cover every post. Paging holds the board
// refresh lock, so a live refresh never appends the same posts at the same
// time; refreshes that arrive meanwhile run as soon as paging ends, and from
// then on new posts append incrementally through the usual live path.
async function loadAllBoardPosts(boardId, generation) {
  const current = () => state.selectedBoardId === boardId && state.boardGeneration === generation;
  while (boardsRefreshing) {
    if (!current()) return;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  if (!current() || !state.boardSnapshot?.hasMore) return;
  boardsRefreshing = true;
  try {
    for (;;) {
      const snapshot = state.boardSnapshot;
      if (!current() || !snapshot?.hasMore) break;
      const known = lastPostSequence(snapshot.posts);
      if (!Number.isSafeInteger(snapshot.nextAfter) || snapshot.nextAfter < known) break;
      snapshot.pagingAll = true;
      await renderBoardDetail(snapshot);
      if (!await loadBoardPosts(boardId, snapshot.nextAfter, true, false)) break;
      if (lastPostSequence(state.boardSnapshot?.posts || []) <= known) break;
    }
  } finally {
    if (state.boardSnapshot) state.boardSnapshot.pagingAll = false;
    boardsRefreshing = false;
    if (current() && state.boardSnapshot) await renderBoardDetail(state.boardSnapshot);
    const pending = boardsRefreshPending;
    boardsRefreshPending = null;
    if (pending) refreshBoards(pending);
  }
}

/* Live board updates. The change stream (below) triggers targeted refreshes as
   agents write. Polling is the fallback: without a stream, poll the selected
   board each tick and the board list and linked boards every third tick; with
   a healthy stream, poll everything only every sixth tick (about 30s). */

// A refresh request is {list, linked}: `linked` is true for every linked board
// on screen, a Set of board IDs for just those, or false for none.
let boardsRefreshing = false;
let boardsRefreshPending = null;

function mergeBoardRequests(a, b) {
  if (!a) return b;
  const linked = a.linked === true || b.linked === true
    ? true
    : (a.linked || b.linked) ? new Set([...(a.linked || []), ...(b.linked || [])]) : false;
  return { list: a.list || b.list, linked };
}

// Requests are serialized so overlapping polls and stream events cannot append
// the same posts twice; a request that arrives mid-refresh runs right after.
async function refreshBoards(request = null) {
  if (document.hidden) return;
  if (!request) {
    state.boardTick += 1;
    if (liveStream.connected && state.boardTick % 6 !== 0) return;
    const slowTick = liveStream.connected || state.boardTick % 3 === 0;
    request = { list: slowTick, linked: slowTick };
  }
  if (boardsRefreshing) {
    boardsRefreshPending = mergeBoardRequests(boardsRefreshPending, request);
    return;
  }
  boardsRefreshing = true;
  try {
    while (request) {
      if (request.list) await refreshBoardList();
      await refreshSelectedBoard(request.linked);
      request = boardsRefreshPending;
      boardsRefreshPending = null;
    }
  } finally {
    boardsRefreshing = false;
  }
}

async function refreshBoardList() {
  try {
    const result = await api('/api/boards');
    state.boards = sortBoards(result.boards);
    renderBoardList();
  } catch { /* Keep the last list; the next slow tick retries. */ }
}

async function refreshSelectedBoard(refreshLinked) {
  const snapshot = state.boardSnapshot;
  const boardId = state.selectedBoardId;
  const generation = state.boardGeneration;
  if (!snapshot || !boardId || snapshot.board?.board_id !== boardId) return;
  // With older pages still unloaded, appending new posts would leave a gap, so
  // only the board record is refreshed until the reader pages forward.
  const params = new URLSearchParams({ limit: '100' });
  const known = lastPostSequence(snapshot.posts);
  if (known) params.set('after_sequence', String(known));
  let changed = false;
  try {
    const result = await api(`/api/boards/${encodeURIComponent(boardId)}?${params}`);
    if (state.selectedBoardId !== boardId || state.boardGeneration !== generation || state.boardSnapshot !== snapshot) return;
    if (result.board && JSON.stringify(result.board) !== JSON.stringify(snapshot.board)) {
      snapshot.board = result.board;
      changed = true;
    }
    const latestSequence = result.latest_sequence ?? snapshot.latestSequence;
    if (latestSequence != null && (snapshot.latestSequence == null || latestSequence > snapshot.latestSequence)) {
      const previousLatest = snapshot.latestSequence ?? known;
      snapshot.latestSequence = latestSequence;
      snapshot.newPosts = Math.max(0, latestSequence - previousLatest);
      if (await loadLatestBoardPosts(boardId)) changed = true;
    }
    if (!snapshot.hasMore) {
      const fresh = (result.posts || []).filter(post => post.sequence > known);
      if (fresh.length) {
        snapshot.posts.push(...fresh);
        if (!snapshot.newPosts) snapshot.newPosts = fresh.length;
        snapshot.hasMore = result.has_more;
        snapshot.nextAfter = result.next_after_sequence;
        changed = true;
      }
    }
    if (refreshLinked && await refreshLinkedBoards(generation, refreshLinked === true ? null : refreshLinked)) changed = true;
  } catch {
    updateBoardLiveMarker(snapshot, true);
    return;
  }
  if (state.boardSnapshot !== snapshot || state.boardGeneration !== generation) return;
  snapshot.checkedAt = Date.now();
  if (changed) {
    snapshot.updatedAt = Date.now();
    await renderBoardDetail(snapshot);
  } else updateBoardLiveMarker(snapshot);
}

// Re-read the child boards currently shown as cards (all, or only the IDs in
// `only`); report whether any changed.
async function refreshLinkedBoards(generation, only = null) {
  const ids = [...state.linkedBoards.keys()].filter(id => !only || only.has(id));
  const results = await Promise.all(ids.map(async id => {
    try {
      const result = await api(`/api/boards/${encodeURIComponent(id)}?limit=100`);
      return [id, { board: result.board, posts: result.posts || [], hasMore: result.has_more }];
    } catch { return [id, null]; }
  }));
  if (state.boardGeneration !== generation) return false;
  let changed = false;
  for (const [id, entry] of results) {
    if (!entry) continue;
    if (JSON.stringify(entry) !== JSON.stringify(state.linkedBoards.get(id))) changed = true;
    state.linkedBoards.set(id, entry);
  }
  return changed;
}

function updateBoardLiveMarker(snapshot, failed = false) {
  const marker = document.querySelector('#board-live');
  if (!marker || !snapshot) return;
  marker.innerHTML = boardLiveMarkerHtml(snapshot, failed);
  bindBoardLiveMarker(snapshot);
}

function boardLiveMarkerHtml(snapshot, failed = false) {
  const checked = snapshot.checkedAt ? new Date(snapshot.checkedAt).toLocaleTimeString() : '—';
  const mode = liveStream.connected ? 'Live · streaming' : 'Polling';
  const status = failed ? `Update failed · last checked ${esc(checked)}` : `${mode} · checked ${esc(checked)}`;
  const fresh = snapshot.newPosts
    ? `<button type="button" class="board-live__new" data-board-new>${fmt(snapshot.newPosts)} new post${snapshot.newPosts === 1 ? '' : 's'}</button>` : '';
  return `${fresh}<span class="board-live__status${failed ? ' is-stale' : ''}">${status}</span>`;
}

function bindBoardLiveMarker(snapshot) {
  document.querySelector('[data-board-new]')?.addEventListener('click', () => {
    snapshot.newPosts = 0;
    const latest = (snapshot.latestPosts || []).filter(post => post.sequence > lastPostSequence(snapshot.posts));
    focusBoardPost(lastPostSequence(latest.length ? latest : snapshot.posts));
    updateBoardLiveMarker(snapshot);
  });
}

/* Extension change stream: notifications stay opaque to the server. The page
   uses fetch() instead of EventSource so it can send the bearer token. */

const liveStream = { connected: false, controller: null, retryMs: 1000, timer: null, queue: [], flushTimer: null };

function updateLiveMarkers() {
  if (state.view === 'boards') updateBoardLiveMarker(state.boardSnapshot);
}

function stopLiveStream() {
  clearTimeout(liveStream.timer);
  liveStream.timer = null;
  liveStream.controller?.abort();
  liveStream.controller = null;
  if (liveStream.connected) {
    liveStream.connected = false;
    updateLiveMarkers();
  }
}

function scheduleLiveStream() {
  if (liveStream.timer || document.hidden) return;
  const delay = liveStream.retryMs;
  liveStream.retryMs = Math.min(liveStream.retryMs * 2, 30000);
  liveStream.timer = setTimeout(() => { liveStream.timer = null; startLiveStream(); }, delay);
}

async function startLiveStream() {
  if (liveStream.controller || document.hidden) return;
  const controller = new AbortController();
  liveStream.controller = controller;
  const token = tokenInput.value.trim().replace(/^Bearer\s+/i, '');
  try {
    const response = await fetch('/api/extensions/events', {
      cache: 'no-store',
      headers: token ? { Authorization: `Bearer ${token}` } : {},
      signal: controller.signal,
    });
    if (!response.ok || !response.body) throw new Error(`${response.status} ${response.statusText}`);
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true }).replace(/\r\n/g, '\n');
      let end;
      while ((end = buffer.indexOf('\n\n')) >= 0) {
        handleLiveRecord(buffer.slice(0, end));
        buffer = buffer.slice(end + 2);
      }
    }
  } catch { /* Aborted, refused, or dropped: fall back to polling below. */ }
  if (liveStream.controller !== controller) return;
  liveStream.controller = null;
  if (liveStream.connected) {
    liveStream.connected = false;
    updateLiveMarkers();
  }
  scheduleLiveStream();
}

function handleLiveRecord(record) {
  let name = 'message';
  let data = '';
  for (const line of record.split('\n')) {
    if (line.startsWith('event:')) name = line.slice(6).trim();
    else if (line.startsWith('data:')) data += line.slice(5).trim();
  }
  if (!data) return; // keep-alive comment
  if (name === 'ready') {
    liveStream.connected = true;
    liveStream.retryMs = 1000;
    // Catch up on anything written while the stream was down.
    liveStream.queue.push({ resync: true });
    updateLiveMarkers();
  } else if (name === 'change') {
    try { liveStream.queue.push(JSON.parse(data)); } catch { return; }
  } else if (name === 'resync') {
    liveStream.queue.push({ resync: true });
  } else return;
  // Agents often write several posts in a burst; refresh once for all of them.
  clearTimeout(liveStream.flushTimer);
  liveStream.flushTimer = setTimeout(flushLiveChanges, 150);
}

function flushLiveChanges() {
  const changes = liveStream.queue.splice(0);
  if (!changes.length || document.hidden) return;
  const resync = changes.some(change => change.resync);
  if (state.view === 'boards') {
    const boardChanges = changes.filter(change => change.kind === 'board');
    const known = new Set(state.boards.map(board => board.board_id));
    const list = resync || boardChanges.some(change => change.tool === 'board_open' || !known.has(change.board_id));
    const linkedIds = new Set(boardChanges.map(change => change.board_id).filter(id => state.linkedBoards.has(id)));
    const selected = boardChanges.some(change => change.board_id === state.selectedBoardId);
    if (resync || list || selected || linkedIds.size) {
      refreshBoards({ list, linked: resync ? true : linkedIds.size ? linkedIds : false });
    }
  }
  if (state.view === 'flows' && (resync || changes.some(change => change.kind === 'flow'))) refreshFlows();
}

tokenInput.addEventListener('change', () => { stopLiveStream(); liveStream.retryMs = 1000; startLiveStream(); });

document.addEventListener('visibilitychange', () => {
  if (document.hidden) stopLiveStream();
  else {
    liveStream.retryMs = 1000;
    startLiveStream();
    refresh();
  }
});

/* ── provider-neutral flow runs ───────────────────────────────── */

function sortFlows(flows) {
  return (flows || []).slice().sort((a, b) => String(a.run?.updated_at || '').localeCompare(String(b.run?.updated_at || '')) || String(a.run?.flow_id || '').localeCompare(String(b.run?.flow_id || ''))).reverse();
}

// Redrawn only when its visible content changes, like the board list.
function renderFlowList() {
  const list = document.querySelector('#flows-list');
  const cards = state.flows.map(({ run = {} }) => {
    const work = flowWorkFor(run);
    const boardError = work ? null : flowBoardCache.get(run.board_id)?.error || null;
    return { run, stage: flowStageName(run, run.stage) || 'No stage', done: work ? work.settledCount : null, total: work ? work.total : null, boardError };
  });
  const signature = JSON.stringify(cards.map(({ run, stage, done, total, boardError }) => [run.flow_id, run.state, stage, run.goal, run.template_version, flowTemplateName(run), done, total, boardError]));
  document.querySelector('#flows-count').textContent = `${fmt(state.flows.length)} flows`;
  if (list.dataset.signature !== signature) {
    list.dataset.signature = signature;
    list.innerHTML = state.flows.length ? cards.map(({ run, stage, done, total, boardError }) => {
      const share = total ? Math.round((done / total) * 100) : 0;
      const progress = total == null
        ? `<span class="flowv-card__progress"><small>${boardError ? 'Board unavailable' : 'Loading tasks…'}</small></span>`
        : total
          ? `<span class="flowv-card__progress" title="${esc(`${done} of ${total} tracked tasks closed`)}"><span class="flowv-bar" role="img" aria-label="${esc(`${done} of ${total} tracked tasks closed`)}"><i style="width:${share}%"></i></span><small>${fmt(done)}/${fmt(total)} closed</small></span>`
          : '<span class="flowv-card__progress"><small>No tracked tasks</small></span>';
      return `<button class="flow-choice flowv-card" type="button" data-flow-id="${esc(run.flow_id || '')}">
        <span class="flow-choice__meta"><span class="flow-state flow-state--${esc(run.state || 'unknown')}">${esc(run.state || 'unknown')}</span><small>${esc(stage)}</small></span>
        <strong class="flowv-clamp" title="${esc(run.goal || '')}">${esc(run.goal || 'Untitled Flow')}</strong>
        ${progress}
      </button>`;
    }).join('') : '<div class="empty-state">No Flows in this workspace yet.</div>';
    list.querySelectorAll('[data-flow-id]').forEach(button => button.addEventListener('click', () => selectFlow(button.dataset.flowId)));
  }
  list.querySelectorAll('.flow-choice').forEach(button => button.classList.toggle('is-selected', button.dataset.flowId === state.selectedFlowId));
}

// Polling fallback for the Flows view, slowed while the change stream is up.
let flowTick = 0;
function pollFlows() {
  flowTick += 1;
  if (liveStream.connected && flowTick % 6 !== 0) return;
  refreshFlows();
}

// Quietly re-read the Flow list and the open Flow; redraw only what changed
// and keep the scroll position.
let flowsRefreshing = false;
async function refreshFlows() {
  if (flowsRefreshing || document.hidden) return;
  flowsRefreshing = true;
  try {
    const result = await api('/api/flows');
    state.flows = sortFlows(result.flows);
    renderFlowList();
    // Board posts carry task status, so a new post on a flow's board can
    // change the view even when the flow itself did not change.
    const boardsChanged = await refreshFlowBoards();
    renderFlowList();
    const flowId = state.selectedFlowId;
    if (!flowId || !state.flows.some(snapshot => snapshot.run?.flow_id === flowId)) return;
    const detail = await api(`/api/flows/${encodeURIComponent(flowId)}`);
    if (state.selectedFlowId !== flowId || state.view !== 'flows') return;
    const next = { run: detail.flow, events: detail.events || [] };
    if (!boardsChanged.has(next.run?.board_id) && JSON.stringify(next) === JSON.stringify(state.flowSnapshot)) return;
    state.flowSnapshot = next;
    const viewport = document.querySelector('.viewport');
    const scrollTop = viewport.scrollTop;
    renderFlowDetail(next);
    viewport.scrollTop = scrollTop;
  } catch { /* Keep what is shown; the next change or poll retries. */ } finally {
    flowsRefreshing = false;
  }
}

async function loadFlows(preferredId = state.selectedFlowId) {
  const list = document.querySelector('#flows-list');
  list.dataset.signature = '';
  list.innerHTML = '<div class="empty-state">Loading Flows…</div>';
  try {
    const result = await api('/api/flows');
    state.flows = sortFlows(result.flows);
    state.selectedFlowId = preferredId;
    renderFlowList();
    refreshFlowBoards().then(() => renderFlowList());
    const next = state.flows.some(snapshot => snapshot.run?.flow_id === preferredId)
      ? preferredId
      : state.flows[0]?.run?.flow_id;
    if (next) await selectFlow(next);
    else {
      state.selectedFlowId = null;
      state.flowSnapshot = null;
      document.querySelector('#flow-detail').innerHTML = '<div class="empty-state">No Flow runs in this workspace.</div>';
    }
  } catch (error) {
    document.querySelector('#flows-count').textContent = '';
    list.innerHTML = `<div class="empty-state">Could not load Flows: ${esc(error.message)}</div>`;
  }
}

async function selectFlow(flowId) {
  state.selectedFlowId = flowId;
  if (state.view === 'flows') history.replaceState(null, '', `#flows/${encodeURIComponent(flowId)}`);
  document.querySelectorAll('.flow-choice').forEach(button => button.classList.toggle('is-selected', button.dataset.flowId === flowId));
  document.querySelector('#flow-detail').innerHTML = '<div class="empty-state">Loading Flow history…</div>';
  try {
    const result = await api(`/api/flows/${encodeURIComponent(flowId)}`);
    if (state.selectedFlowId !== flowId) return;
    state.flowSnapshot = { run: result.flow, events: result.events || [] };
    renderFlowDetail(state.flowSnapshot);
    const boardId = state.flowSnapshot.run?.board_id;
    if (boardId && !flowBoardCache.get(boardId)?.loaded) {
      await loadFlowBoard(boardId);
      if (state.selectedFlowId === flowId && state.view === 'flows') {
        renderFlowDetail(state.flowSnapshot);
        renderFlowList();
      }
    }
  } catch (error) {
    if (state.selectedFlowId === flowId) document.querySelector('#flow-detail').innerHTML = `<div class="empty-state">Could not load Flow: ${esc(error.message)}</div>`;
  }
}

function flowTaskEntries(tasks) {
  if (Array.isArray(tasks)) return tasks;
  if (tasks && typeof tasks === 'object') return Object.entries(tasks).map(([taskId, task]) => ({ task_id: taskId, ...task }));
  return [];
}

/* Flow boards. Task status lives in the posts of a flow's board, so each
   flow board is read once in pages of 100, then caught up with after_sequence,
   and rolled up with rollupTasks. One cache entry per board. */
const flowBoardCache = new Map();
const flowBoardLoads = new Map();

function flowGroupComponent(board) {
  const components = board?.board_template?.snapshot?.presentation?.components;
  return Array.isArray(components) ? components.find(component => component?.kind === 'post_groups') || {} : {};
}

// Resolves to true when new posts arrived.
function loadFlowBoard(boardId) {
  if (!boardId) return Promise.resolve(false);
  if (flowBoardLoads.has(boardId)) return flowBoardLoads.get(boardId);
  const load = (async () => {
    const entry = flowBoardCache.get(boardId) || { board: null, posts: [], latest: 0, rollup: null, loaded: false, error: null };
    const previousError = entry.error;
    let after = entry.latest;
    let fresh = [];
    try {
      for (let page = 0; page < 50; page += 1) {
        const query = `limit=100${after ? `&after_sequence=${after}` : ''}`;
        const result = await api(`/api/boards/${encodeURIComponent(boardId)}?${query}`);
        entry.board = result.board || entry.board;
        const posts = result.posts || [];
        fresh = fresh.concat(posts);
        const last = posts.length ? posts[posts.length - 1].sequence : after;
        if (!result.has_more || !posts.length) break;
        after = Number(result.next_after_sequence) || last;
      }
      entry.error = null;
    } catch (error) {
      entry.error = error.message;
    }
    // A board that has never loaded stays unloaded on error, so the flow shows
    // the error instead of an empty task list.
    if (entry.error && !entry.loaded && !fresh.length) {
      flowBoardCache.set(boardId, entry);
      return entry.error !== previousError;
    }
    const changed = fresh.length > 0 || !entry.loaded || entry.error !== previousError;
    if (fresh.length) {
      entry.posts = mergeFlowBoardPosts(entry.posts, fresh);
      entry.latest = entry.posts.length ? entry.posts[entry.posts.length - 1].sequence : 0;
    }
    if (changed) entry.rollup = rollupFlowBoardTasks(entry.posts, boardId, flowGroupComponent(entry.board));
    entry.loaded = true;
    flowBoardCache.set(boardId, entry);
    return changed;
  })();
  flowBoardLoads.set(boardId, load);
  return load.finally(() => flowBoardLoads.delete(boardId));
}

// Catch up every board a listed flow uses. Returns the IDs that changed.
async function refreshFlowBoards() {
  const ids = [...new Set(state.flows.map(snapshot => snapshot.run?.board_id).filter(Boolean))];
  const results = await Promise.all(ids.map(id => loadFlowBoard(id).then(changed => [id, changed])));
  return new Set(results.filter(([, changed]) => changed).map(([id]) => id));
}

// Tracked tasks for a flow, or null while its board has not loaded. The tile
// groups, tones and order come from workGroups over the board's own declared
// groups, so the Flow tiles and the board tiles always agree.
function flowWorkFor(run) {
  const entry = flowBoardCache.get(run?.board_id);
  if (!entry?.loaded || !entry.rollup) return null;
  const work = flowTrackedWork(run.references || [], entry.rollup, run.board_id);
  const groups = workGroups(work, flowGroupComponent(entry.board));
  const toneByValue = new Map(groups.filter(group => group.value !== null).map(group => [group.value, group.tone]));
  return {
    ...work,
    tileGroups: groups,
    settledCount: groups.filter(group => group.settled).reduce((sum, group) => sum + group.count, 0),
    toneOf: value => toneByValue.get(value) || 'other'
  };
}

function flowStageName(run, stageId) {
  if (stageId == null || stageId === '') return '';
  const stage = (run?.flow_template?.snapshot?.stages || []).find(item => String(item?.id) === String(stageId));
  return stage?.name ? String(stage.name) : String(stageId).replaceAll('_', ' ');
}

const flowRelativeFormat = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });
function flowTime(value) {
  const time = new Date(value).getTime();
  if (!value || !Number.isFinite(time)) return '<time>time unavailable</time>';
  const delta = (time - Date.now()) / 1000;
  const units = [['year', 31536000], ['month', 2592000], ['week', 604800], ['day', 86400], ['hour', 3600], ['minute', 60]];
  const unit = units.find(([, size]) => Math.abs(delta) >= size);
  const text = unit ? flowRelativeFormat.format(Math.round(delta / unit[1]), unit[0]) : 'just now';
  return `<time datetime="${esc(value)}" title="${esc(new Date(value).toLocaleString())}">${esc(text)}</time>`;
}

// A titled button that opens a post or board on the Boards view.
function flowTargetLink(targetId, relation = null) {
  const target = parseFlowLinkTarget(targetId);
  const relationHtml = relation ? ` <span class="flowv-rel">(${esc(String(relation).replaceAll('_', ' '))})</span>` : '';
  if (target.kind === 'post' && target.boardId && target.sequence) {
    const entry = flowBoardCache.get(target.boardId);
    const post = entry?.posts.find(item => item.sequence === target.sequence) || null;
    const label = flowPostLabel(post, target.sequence);
    const where = entry ? '' : ` · ${flowBoardLabel(target.boardId)}`;
    return `<button type="button" class="flowv-link" data-flow-board-id="${esc(target.boardId)}" data-flow-post-sequence="${target.sequence}" title="${esc(label)}">${esc(label)}${esc(where)}</button>${relationHtml}`;
  }
  if (target.kind === 'board') return `<button type="button" class="flowv-link" data-flow-board-id="${esc(target.boardId)}">Board · ${esc(flowBoardLabel(target.boardId))}</button>${relationHtml}`;
  return `<span class="flowv-ref">${esc(targetId)}</span>${relationHtml}`;
}

function flowBoardLabel(boardId) {
  const cached = flowBoardCache.get(boardId)?.board;
  if (cached?.title || cached?.key) return String(cached.title || cached.key);
  const board = state.boards.find(item => item.board_id === boardId);
  return board ? `${board.title || board.key} · ${board.type || 'general'}` : boardId;
}

function flowPostLink(postId, label = 'Linked board post') {
  const target = String(postId || '');
  const marker = ':post:';
  const separator = target.lastIndexOf(marker);
  const boardId = separator > 0 ? target.slice(0, separator) : '';
  const sequence = separator > 0 ? Number(target.slice(separator + marker.length)) : NaN;
  if (!boardId || !Number.isInteger(sequence) || sequence < 1) return `<span class="mono">${esc(label)} · ${esc(target || 'post unavailable')}</span>`;
  return `<button type="button" class="board-link" data-flow-board-id="${esc(boardId)}" data-flow-post-sequence="${sequence}">${esc(label)} · ${esc(flowBoardLabel(boardId))} · post ${sequence}</button>`;
}

// FlowRuns pin their AgentFlow template snapshot; legacy runs only carry the ID.
function flowTemplateName(run) {
  return run.flow_template?.snapshot?.name || run.template_id || 'Unknown template';
}

// Flows whose full audit history is expanded ("Show all").
const flowEventsExpanded = new Set();
const FLOW_EVENTS_SHOWN = 10;

function renderFlowDetail(snapshot) {
  const run = snapshot.run || {};
  const events = snapshot.events || [];
  const tasks = flowTaskEntries(run.tasks);
  const boardEntry = flowBoardCache.get(run.board_id);
  const work = flowWorkFor(run);
  const stageName = id => flowStageName(run, id);

  // Header: goal, state, template, coordinator and times. Hash and revision
  // sit in the Details disclosure.
  const template = run.flow_template || {};
  const headHtml = `<header class="flowv-head">
      <p class="eyebrow">${esc(flowTemplateName(run))} · v${esc(run.template_version ?? template.version ?? '—')}</p>
      <h2 class="flowv-goal flowv-clamp" title="${esc(run.goal || '')}">${esc(run.goal || 'Untitled Flow')}</h2>
      <div class="flowv-meta">
        <span class="flow-state flow-state--${esc(run.state || 'unknown')}">${esc(run.state || 'unknown')}</span>
        <span><small>Coordinator</small> ${esc(run.coordinator?.agent_id || 'Unclaimed')}</span>
        <span><small>Created</small> ${flowTime(run.created_at)}${run.created_by ? ` by ${esc(run.created_by)}` : ''}</span>
        <span><small>Updated</small> ${flowTime(run.updated_at)}</span>
      </div>
    </header>`;

  // Stage stepper from the pinned template's stages.
  const steps = flowStageSteps(template.snapshot?.stages || [], run.stage, run.state, events);
  const stepLabel = { done: 'Done', current: 'Current', upcoming: 'Upcoming' };
  const stepperHtml = steps.length ? `<ol class="flowv-stepper" aria-label="Stages">${steps.map((step, index) => `
      <li class="flowv-step flowv-step--${esc(step.state)}"${step.description ? ` title="${esc(step.description)}"` : ''}${step.state === 'current' ? ' aria-current="step"' : ''}>
        <span class="flowv-step__dot">${step.state === 'done' ? '✓' : fmt(index + 1)}</span>
        <span class="flowv-step__text"><strong>${esc(step.name)}</strong><small>${esc(stepLabel[step.state])}</small></span>
      </li>`).join('')}</ol>` : '';

  // Tile row of tracked tasks, one tile per board group that has tasks, with
  // the board's labels and tones.
  const shownGroups = work ? work.tileGroups.filter(group => group.count) : [];
  const tilesHtml = work
    ? (shownGroups.length ? `<div class="kpis board-kpis flowv-kpis">${shownGroups.map(group => `
      <article class="kpi flowv-kpi"><span><i class="group-dot group-tone--${esc(group.tone)}" aria-hidden="true"></i>${esc(group.label)}</span><strong>${fmt(group.count)}</strong><small>of ${fmt(work.total)} tracked</small></article>`).join('')}</div>` : '')
    : `<div class="kpis board-kpis flowv-kpis"><article class="kpi flowv-kpi"><span>Tracked tasks</span><strong>—</strong><small>${boardEntry?.error ? 'board unavailable' : 'loading'}</small></article></div>`;

  // Tracked work: one row per root task the flow references.
  let workHtml;
  if (!work) {
    workHtml = boardEntry?.error
      ? `<div class="empty-state">Could not load the flow board: ${esc(boardEntry.error)}</div>`
      : '<div class="empty-state">Loading tracked tasks…</div>';
  } else if (!work.tasks.length) {
    workHtml = '<div class="empty-state">This flow does not reference any task posts yet.</div>';
  } else {
    workHtml = `<ul class="flowv-tasks">${work.tasks.map(task => {
      const latest = task.latest && task.latest.sequence !== task.sequence
        ? `<span class="flowv-task__latest">Latest: #${fmt(task.latest.sequence)} · ${esc(task.latest.title)} · ${flowTime(task.latest.createdAt)}</span>`
        : `<span class="flowv-task__latest">Posted ${flowTime(task.post?.created_at)}</span>`;
      return `<li><button type="button" class="flowv-task" data-flow-board-id="${esc(run.board_id)}" data-flow-post-sequence="${task.sequence}" title="${esc(`Open #${task.sequence} on the board`)}">
          ${statusPill(task.statusLabel, work.toneOf(task.status))}
          <span class="flowv-task__main"><strong>${esc(task.title)}</strong>${latest}</span>
          <span class="flowv-task__side"><span>#${fmt(task.sequence)}</span><span>${esc(task.assignee || 'Unassigned')}</span></span>
        </button></li>`;
    }).join('')}</ul>`;
  }
  const otherRefs = work ? work.otherReferences : [];
  const otherHtml = otherRefs.length
    ? `<div class="flowv-other"><small>Other references</small><ul>${otherRefs.map(ref => `<li>${flowTargetLink(ref.id, ref.relations.join(', '))}</li>`).join('')}</ul></div>`
    : '';
  const workNote = `From ${esc(flowBoardLabel(run.board_id))} · status rolled up from board posts`;
  const workSection = `<section class="flowv-section">
      <div class="panel__head"><div><p class="eyebrow">Tracked work</p><h3>${work ? `${fmt(work.total)} ${work.total === 1 ? 'task' : 'tasks'}` : 'Tasks'}</h3></div>
        <button type="button" class="flowv-note-link" data-flow-board-id="${esc(run.board_id)}">${workNote}</button></div>
      ${workHtml}${otherHtml}
    </section>`;

  // Audit history: one readable line per event, newest first.
  const newestFirst = events.slice().reverse();
  const expanded = flowEventsExpanded.has(run.flow_id);
  const shownEvents = expanded ? newestFirst : newestFirst.slice(0, FLOW_EVENTS_SHOWN);
  const historyHtml = shownEvents.length ? `<ol class="flowv-events">${shownEvents.map(event => {
    const summary = flowEventSummary(event, stageName);
    const details = summary.details.map(([label, text, value]) => {
      const target = typeof value === 'string' ? parseFlowLinkTarget(value) : null;
      const valueHtml = target && (target.kind === 'post' || (target.kind === 'board' && value.startsWith('board:'))) ? flowTargetLink(value) : esc(text);
      return `<div><dt>${esc(label)}</dt><dd>${valueHtml}</dd></div>`;
    }).join('');
    const postHtml = event.post_id ? ` · ${flowTargetLink(event.post_id)}` : '';
    return `<li class="flowv-event">
        <div class="flowv-event__line"><p><strong>${esc(event.actor || 'Unknown actor')}</strong> ${esc(summary.text)}${summary.reference ? ` ${flowTargetLink(summary.reference.id, summary.reference.relation)}` : ''}${postHtml}</p>${flowTime(event.timestamp)}</div>
        ${details ? `<dl class="flowv-event__data">${details}</dl>` : ''}
      </li>`;
  }).join('')}</ol>` : '<div class="empty-state">No Flow events recorded yet.</div>';
  const moreHtml = newestFirst.length > FLOW_EVENTS_SHOWN
    ? `<button type="button" class="btn flowv-more" data-flow-show-all="${esc(run.flow_id || '')}">${expanded ? 'Show latest 10' : `Show all ${fmt(newestFirst.length)} events`}</button>`
    : '';
  const historySection = `<section class="flowv-section">
      <div class="panel__head"><div><p class="eyebrow">Audit history</p><h3>${fmt(events.length)} recorded ${events.length === 1 ? 'event' : 'events'}</h3></div><span class="panel__note">Newest first · recorded by Membrick</span></div>
      ${historyHtml}${moreHtml}
    </section>`;

  const detailsHtml = `<details class="flowv-details"><summary>Details</summary><dl>
      <div><dt>Flow ID</dt><dd>${esc(run.flow_id || '—')}</dd></div>
      <div><dt>Template</dt><dd>${esc(template.template_id || run.template_id || '—')} · v${esc(run.template_version ?? '—')}</dd></div>
      <div><dt>Content hash</dt><dd>${esc(template.content_hash || '—')}</dd></div>
      <div><dt>Revision</dt><dd>${fmt(run.revision || 0)}</dd></div>
      <div><dt>Coordinator</dt><dd>${esc(run.coordinator?.agent_id || 'Unclaimed')}${run.coordinator?.provider ? ` · ${esc(run.coordinator.provider)}` : ''} · fence ${fmt(run.coordinator_fence || 0)}</dd></div>
      <div><dt>Board</dt><dd>${esc(run.board_id || '—')}</dd></div>
    </dl></details>`;

  // Legacy runs that recorded tasks on the flow itself.
  const legacyHtml = tasks.length ? legacyFlowTasksHtml(tasks, events) : '';

  document.querySelector('#flow-detail').innerHTML = `<div class="flowv">${headHtml}${stepperHtml}${tilesHtml}${workSection}${legacyHtml}${historySection}${detailsHtml}</div>`;
  document.querySelectorAll('[data-flow-show-all]').forEach(button => button.addEventListener('click', () => {
    const flowId = button.dataset.flowShowAll;
    if (flowEventsExpanded.has(flowId)) flowEventsExpanded.delete(flowId);
    else flowEventsExpanded.add(flowId);
    renderFlowDetail(state.flowSnapshot || snapshot);
  }));
  bindFlowBoardLinks();
}

function legacyFlowTasksHtml(tasks, events) {
  const eventByTask = new Map();
  for (const event of events) if (event.task_id) eventByTask.set(event.task_id, [...(eventByTask.get(event.task_id) || []), event]);
  const taskHtml = tasks.length ? tasks.map(task => {
    const taskEvents = eventByTask.get(task.task_id) || [];
    const review = taskEvents.slice().reverse().find(event => ['review_approved', 'review_changes_requested'].includes(event.kind));
    const assignee = task.assignee?.agent_id ? `${task.assignee.agent_id} · ${task.assignee.provider || 'agent'}` : 'Unassigned';
    const evidence = task.evidence || [];
    const postIds = [...new Set(task.post_ids || [])];
    return `<article class="flow-task">
      <div class="flow-task__head"><div><p class="eyebrow">Task · ${esc(task.task_id || 'unknown id')}</p><h3>${esc(task.title || task.task_id || 'Untitled task')}</h3></div><span class="flow-state flow-state--${esc(task.state || 'unknown')}">${esc(task.state || 'unknown')}</span></div>
      <div class="flow-task__meta"><span><small>Assigned</small>${esc(assignee)}</span><span><small>Attempts</small>${fmt(task.attempts || 0)}</span><span><small>Current attempt</small>${esc(task.attempt_id || '—')}</span><span><small>Revision</small>${fmt(task.revision || 0)}</span></div>
      ${task.summary ? `<p class="flow-task__summary">${esc(task.summary)}</p>` : ''}
      ${task.blocker ? `<p class="flow-task__blocker"><strong>Blocker</strong> ${esc(task.blocker)}</p>` : ''}
      ${review ? `<p class="flow-task__review"><strong>Latest review</strong> ${esc(review.kind.replaceAll('_', ' '))} · ${esc(review.actor || 'unknown reviewer')}</p>` : ''}
      ${postIds.length ? `<div class="flow-task__posts"><small>Linked task and report posts</small>${postIds.map(postId => flowPostLink(postId)).join('')}</div>` : ''}
      ${evidence.length ? `<div class="flow-evidence"><small>Evidence</small>${evidence.map(item => `<span>${esc(item)}</span>`).join('')}</div>` : ''}
    </article>`;
  }).join('') : '';
  return `<section class="flowv-section"><div class="panel__head"><div><p class="eyebrow">Recorded on the flow</p><h3>Flow tasks · ${fmt(tasks.length)}</h3></div></div><div class="flow-tasks">${taskHtml}</div></section>`;
}

// Every flow link opens its board, and its post when it names one, on the
// Boards view.
function bindFlowBoardLinks() {
  document.querySelectorAll('#flow-detail [data-flow-board-id]').forEach(button => button.addEventListener('click', async () => {
    const boardId = button.dataset.flowBoardId;
    const postSequence = button.dataset.flowPostSequence ? Number(button.dataset.flowPostSequence) : null;
    showView('boards', false);
    state.boardTrail = [];
    if (!state.boards.length) {
      try { state.boards = sortBoards((await api('/api/boards')).boards); } catch { /* The board view reports its own load error. */ }
    }
    state.selectedBoardId = boardId;
    renderBoardList();
    await selectBoard(boardId, postSequence);
  }));
}

// A link target is a board ID, a post ID (`<board_id>:post:<sequence>`), a web
// URL, or an opaque reference. Only the target's syntax decides how it renders.
function parseLinkTarget(target) {
  return parseBoardLinkTarget(target, state.boards.map(board => board.board_id));
}

// The board on screen and the titles of its loaded posts, so a link to a post
// on the same board reads as that post's title without the board name.
let boardLinkContext = { boardId: null, titles: new Map() };

const relationWords = relation => String(relation || 'related').replaceAll('_', ' ');

// A board ID ends in its key; that key is the readable fallback when the
// board's title is not known, so a raw board ID is never shown.
function boardKeyFromId(boardId) {
  const value = String(boardId || '');
  return value.slice(value.lastIndexOf(':') + 1) || value;
}

function boardLabel(boardId) {
  const board = state.boards.find(item => item.board_id === boardId) || state.linkedBoards.get(boardId)?.board;
  return board ? board.title || board.key : boardKeyFromId(boardId);
}

function linkTargetLabel(parsed, context = boardLinkContext) {
  const sameBoard = parsed.boardId === context.boardId;
  if (parsed.kind === 'board') return sameBoard ? 'this board' : boardLabel(parsed.boardId);
  let title = sameBoard ? context.titles.get(parsed.sequence) : null;
  if (!sameBoard) {
    const linkedPost = state.linkedBoards.get(parsed.boardId)?.posts?.find(post => post.sequence === parsed.sequence);
    if (linkedPost) title = postTitleParts(linkedPost).title;
  }
  const postText = title || `post ${parsed.sequence}`;
  return sameBoard ? postText : `${boardLabel(parsed.boardId)} · ${postText}`;
}

// Links read as "<relation> <target title>". More than `limit` links fold
// into a "+N more" toggle.
function renderBoardLinks(links = [], context = boardLinkContext, limit = 3) {
  if (!links.length) return '<span class="muted">No linked items</span>';
  const items = links.map(link => {
    const target = String(link.target || '');
    const parsed = parseLinkTarget(target);
    const navigable = parsed.boardId && parsed.boardId.startsWith('board:') && (parsed.kind === 'board' || parsed.sequence);
    let item;
    if (navigable) {
      const label = linkTargetLabel(parsed, context);
      item = `<button type="button" class="board-link" title="${esc(label)}" data-board-link="${esc(parsed.boardId)}"${parsed.sequence ? ` data-post-sequence="${parsed.sequence}"` : ''}>${esc(label)}</button>`;
    } else if (parsed.kind === 'external') {
      item = `<a class="board-link" href="${esc(target)}" target="_blank" rel="noopener noreferrer">${esc(target)}</a>`;
    } else {
      item = `<span class="board-link board-link--plain">${esc(target.startsWith('repo:') ? target.slice(5) : target)}</span>`;
    }
    return `<span class="board-link-item board-link-item--${parsed.kind}"><small>${esc(relationWords(link.relation))}</small>${item}</span>`;
  });
  const rest = items.slice(limit);
  return `<div class="board-links">${items.slice(0, limit).join('')}${rest.length ? `<details class="board-links__more"><summary>+${fmt(rest.length)} more</summary><div class="board-links">${rest.join('')}</div></details>` : ''}</div>`;
}

function bindBoardLinkButtons(root) {
  root.querySelectorAll('[data-board-link]').forEach(button => button.addEventListener('click', () => navigateBoardLink(
    button.dataset.boardLink,
    button.dataset.postSequence ? Number(button.dataset.postSequence) : null,
  )));
}

// In-place navigation keeps a trail back to where the reader started.
async function navigateBoardLink(boardId, postSequence) {
  if (boardId === state.selectedBoardId) {
    if (postSequence == null) return;
    if (document.querySelector(`#post-${postSequence}`) || boardTaskPosts.has(postSequence)) await focusBoardPost(postSequence);
    else await selectBoard(boardId, postSequence);
    return;
  }
  const current = state.boardSnapshot?.board;
  if (current) state.boardTrail = [...state.boardTrail, { board_id: current.board_id, label: current.title || current.key }].slice(-20);
  await selectBoard(boardId, postSequence);
}

function renderBoardTrail(board) {
  if (!state.boardTrail.length) return '';
  const previous = state.boardTrail.at(-1);
  return `<nav class="board-trail" aria-label="Board path">
    <button type="button" class="board-trail__back" data-trail-index="${state.boardTrail.length - 1}">← Back to ${esc(previous.label)}</button>
    <ol>${state.boardTrail.map((entry, index) => `<li><button type="button" class="board-link" data-trail-index="${index}">${esc(entry.label)}</button></li>`).join('')}<li aria-current="page">${esc(board.title || board.key)}</li></ol>
  </nav>`;
}

// Count tasks (not posts) per declared group, using the task rollup. Tasks
// whose status is missing or undeclared are counted as "Other", never guessed.
// Each group's `posts` holds its tasks, so groupSummaryHtml can count them.
function groupPosts(component, posts, boardId) {
  const declared = Array.isArray(component.groups) ? component.groups.filter(group => group && typeof group === 'object' && Object.hasOwn(group, 'value')) : [];
  const rollup = rollupTasks(posts, boardId, component);
  // A group's color comes only from a tone the template declares; otherwise it
  // gets a neutral color by position, so no meaning is read into its value.
  const groups = rollup.groups.map((group, index) => ({
    label: group.label,
    tone: GROUP_TONES.has(declared[index]?.tone) ? declared[index].tone : `n${index % 6}`,
    posts: group.tasks,
  }));
  return { declared: typeof component.group_by === 'string' && declared.length > 0, groups, other: rollup.other.tasks, total: rollup.total };
}

const GROUP_TONES = new Set(['neutral', 'info', 'success', 'warning', 'danger']);

function groupSummaryHtml(grouping, partial) {
  if (!grouping.declared || !grouping.total) return '';
  const segments = [...grouping.groups];
  if (grouping.other.length) segments.push({ label: 'Other', posts: grouping.other, tone: 'other' });
  const shown = segments.filter(segment => segment.posts.length);
  return `<div class="group-summary">
    <div class="group-bar" role="img" aria-label="${esc(shown.map(segment => `${segment.label} ${segment.posts.length}`).join(', '))}">${shown.map(segment => `<span class="group-bar__seg group-tone--${segment.tone}" style="flex-grow:${segment.posts.length}"></span>`).join('')}</div>
    <div class="group-legend">${segments.map(segment => `<span><i class="group-tone--${segment.tone}"></i>${esc(segment.label)} <b>${fmt(segment.posts.length)}</b></span>`).join('')}<span class="muted">${fmt(grouping.total)} ${partial ? 'loaded' : 'total'}</span></div>
  </div>`;
}

function fieldIndex(schema) {
  const fields = schema?.fields;
  if (!Array.isArray(fields)) return new Map();
  return new Map(fields.filter(field => field && typeof field.id === 'string').map(field => [field.id, field]));
}

function displayValue(value, depth = 0) {
  if (value == null) return '—';
  if (typeof value === 'string') return value.length > 8000 ? `${value.slice(0, 8000)}…` : value;
  if (typeof value === 'number' || typeof value === 'boolean') return String(value);
  if (depth >= 4) return '[nested value]';
  if (Array.isArray(value)) return value.slice(0, 100).map(item => displayValue(item, depth + 1)).join(', ');
  if (typeof value === 'object') return JSON.stringify(value, null, 2).slice(0, 8000);
  return String(value);
}

function fieldRows(fields, schema, selectedIds = null) {
  if (!fields || typeof fields !== 'object' || Array.isArray(fields)) return '';
  const definitions = fieldIndex(schema);
  const ids = Array.isArray(selectedIds)
    ? selectedIds.filter(id => typeof id === 'string')
    : [...new Set([...definitions.keys(), ...Object.keys(fields)])];
  // Empty values are left out rather than shown as blank rows.
  const rows = ids.filter(id => Object.hasOwn(fields, id) && !isEmptyValue(fields[id])).map(id => {
    const definition = definitions.get(id);
    return `<div><dt>${esc(definition?.label || id.replaceAll('_', ' '))}</dt><dd>${fieldValueHtml(fields[id])}</dd></div>`;
  });
  return rows.join('');
}

function isEmptyValue(value) {
  return value == null || (typeof value === 'string' && !value.trim()) || (Array.isArray(value) && value.length === 0);
}

// A list of plain values reads better as a list than as comma-joined text.
function fieldValueHtml(value) {
  if (Array.isArray(value) && value.length > 1 && value.every(item => item == null || typeof item !== 'object')) {
    return `<ul class="field-values">${value.slice(0, 100).map(item => `<li>${esc(displayValue(item))}</li>`).join('')}</ul>`;
  }
  return esc(displayValue(value));
}

function postTemplateFor(instance, post) {
  return instance?.snapshot?.post_templates?.find(item => item.template_id === post.post_template_id && item.version === post.post_template_version) || null;
}

function postMatches(component, post) {
  const ids = component.post_template_ids;
  return !Array.isArray(ids) || ids.length === 0 || ids.includes(post.post_template_id);
}

function renderPostCard(post, instance, compact = false, selectedFieldIds = null, anchor = false) {
  const postTemplate = postTemplateFor(instance, post);
  const schema = postTemplate?.schema;
  const presentation = postTemplate?.presentation;
  const postComponents = Array.isArray(presentation?.components) ? presentation.components : [];
  const fieldComponent = postComponents.find(item => item?.kind === 'fields');
  const heroComponent = postComponents.find(item => item?.kind === 'hero');
  const unsupportedComponents = Array.isArray(presentation?.components)
    ? presentation.components.filter(item => !['fields', 'hero'].includes(item?.kind)) : [];
  const heroTitle = heroComponent?.title_field && Object.hasOwn(post.fields || {}, heroComponent.title_field)
    ? displayValue(post.fields[heroComponent.title_field]) : null;
  const title = post.title || heroTitle || `Post ${post.sequence}`;
  const anchorId = anchor ? ` id="post-${esc(post.sequence)}"` : '';
  // Each value is shown once: a field equal to the title is left out, and the
  // narrative is left out when a shown field already says the same thing.
  const fields = post.fields && typeof post.fields === 'object' && !Array.isArray(post.fields) ? post.fields : {};
  const sameText = (a, b) => String(a ?? '').trim() === String(b ?? '').trim();
  const visible = ids => (Array.isArray(ids) ? ids : Object.keys(fields)).filter(id => typeof id === 'string' && !sameText(displayValue(fields[id]), title));
  const selectedIds = visible(Array.isArray(selectedFieldIds) ? selectedFieldIds : fieldComponent?.field_ids);
  const summaryIds = heroComponent ? visible(heroComponent.summary_fields).filter(id => !selectedIds.includes(id)) : [];
  const shownValues = [...selectedIds, ...summaryIds].filter(id => Object.hasOwn(fields, id)).map(id => displayValue(fields[id]));
  if (compact === 'timeline') {
    // Activity entries are compact: title, author, time and links only.
    return `<article class="template-post template-post--timeline"${anchorId}>
    <div class="template-post__head"><button type="button" class="board-link template-post__title" data-post-focus="${esc(post.sequence)}">${esc(title)}</button><span class="template-post__meta">${agentChip(post.author_agent_id || post.provider || 'agent')}${timeHtml(post.created_at)}</span></div>
    ${(post.links || []).length ? renderBoardLinks(post.links) : ''}
  </article>`;
  }
  const rows = fieldRows(fields, schema, selectedIds);
  const summaryRows = summaryIds.length ? fieldRows(fields, schema, summaryIds) : '';
  const narrative = post.content && !shownValues.some(value => sameText(value, post.content)) && !sameText(post.content, title)
    ? `<p class="template-post__content">${esc(post.content)}</p>` : '';
  return `<article class="template-post${compact ? ' template-post--compact' : ''}"${anchorId}>
    <div class="template-post__head"><button type="button" class="board-link template-post__title" data-post-focus="${esc(post.sequence)}">${esc(title)}</button><span class="template-post__meta">${agentChip(post.author_agent_id || post.provider || 'agent')}${timeHtml(post.created_at)}</span></div>
    ${summaryRows ? `<dl class="template-fields">${summaryRows}</dl>` : ''}
    ${rows ? `<dl class="template-fields">${rows}</dl>` : ''}${narrative}
    ${unsupportedComponents.map(item => `<span class="template-unsupported-note">Additional presentation: ${esc(item?.kind || 'unknown')}</span>`).join('')}
    ${(post.links || []).length ? renderBoardLinks(post.links) : ''}
  </article>`;
}

// Linked child boards are read with their first page of posts so a card can
// summarize the child's own declared post groups. Entries are cached and
// re-read by the slow live-update tick.
async function linkedBoardEntry(target, used) {
  used?.add(target);
  if (state.linkedBoards.has(target)) return state.linkedBoards.get(target);
  try {
    const result = await api(`/api/boards/${encodeURIComponent(target)}?limit=100`);
    const entry = { board: result.board, posts: result.posts || [], hasMore: result.has_more };
    state.linkedBoards.set(target, entry);
    return entry;
  } catch {
    return null;
  }
}

async function linkedBoardCards(board, component, used) {
  const links = (board.links || []).filter(link => {
    const target = String(link.target || '');
    return parseLinkTarget(target).kind === 'board' && (!component.relation || link.relation === component.relation);
  });
  const cards = await Promise.all(links.map(async link => {
    const target = String(link.target || '');
    const entry = target.startsWith('board:') ? await linkedBoardEntry(target, used) : null;
    const linked = entry?.board || state.boards.find(item => item.board_id === target);
    const label = linked?.title || linked?.key || target;
    const linkedInstance = linked?.board_template;
    const linkedSnapshot = linkedInstance?.snapshot;
    const childComponents = Array.isArray(linkedSnapshot?.presentation?.components) ? linkedSnapshot.presentation.components : [];
    const childHero = childComponents.find(item => item?.kind === 'hero');
    const childFields = childComponents.find(item => item?.kind === 'fields');
    const childGroups = childComponents.find(item => item?.kind === 'post_groups');
    const childSummaryFields = Array.isArray(childHero?.summary_fields) ? childHero.summary_fields : [];
    const selectedFields = Array.isArray(component.field_ids)
      ? component.field_ids
      : childHero ? [childHero.title_field, ...childSummaryFields].filter(Boolean)
        : childFields?.field_ids;
    const rows = fieldRows(linked?.fields, linkedSnapshot?.schema, selectedFields);
    const summary = childGroups && entry ? groupSummaryHtml(groupPosts(childGroups, entry.posts, entry.board?.board_id || target), entry.hasMore) : '';
    const meta = linkedInstance
      ? `${linkedSnapshot?.name || linkedInstance.template_id} v${linkedInstance.version}`
      : entry ? `${fmt(entry.posts.length)}${entry.hasMore ? '+' : ''} posts` : '';
    const action = linked
      ? `<button type="button" class="board-link" data-board-link="${esc(target)}">${esc(label)}</button>`
      : `<span class="board-link board-link--plain">${esc(label)}</span>`;
    return `<article class="template-reference-card"><small>${esc(link.relation || 'related')}</small>${action}${meta ? `<span class="template-reference-card__meta">${esc(meta)}</span>` : ''}${summary}${rows ? `<dl class="template-fields">${rows}</dl>` : ''}</article>`;
  }));
  return cards.length ? cards.join('') : '<div class="empty-state">No linked boards for this section.</div>';
}

async function renderTemplateComponent(component, index, board, posts, instance, hasMorePosts, used) {
  if (!component || typeof component.kind !== 'string') return '';
  const title = component.title ? `<h3>${esc(component.title)}</h3>` : '';
  const fields = board.fields || {};
  if (component.kind === 'hero') {
    const titleValue = component.title_field && Object.hasOwn(fields, component.title_field)
      ? displayValue(fields[component.title_field]) : board.title || board.key;
    const summaries = Array.isArray(component.summary_fields) ? component.summary_fields : [];
    const rows = fieldRows(fields, instance?.snapshot?.schema, summaries);
    return `<section class="template-hero"><p class="eyebrow">${esc(instance?.snapshot?.name || instance?.template_id || 'Template instance')} · v${esc(instance?.version || '')}</p><h2>${esc(titleValue)}</h2>${rows ? `<dl class="template-fields template-hero__fields">${rows}</dl>` : ''}</section>`;
  }
  if (component.kind === 'fields') {
    const rows = fieldRows(fields, instance?.snapshot?.schema, component.field_ids);
    return rows ? `<section class="template-section">${title}<dl class="template-fields">${rows}</dl></section>` : '';
  }
  if (component.kind === 'post_list' || component.kind === 'post_timeline' || component.kind === 'post_groups') {
    const matching = posts.filter(post => postMatches(component, post));
    if (component.kind === 'post_groups') {
      const declaredGroups = typeof component.group_by === 'string' && Array.isArray(component.groups)
        && component.groups.some(group => group && typeof group === 'object' && Object.hasOwn(group, 'value'));
      if (!declaredGroups) {
        // Without declared values there is nothing to group by: show a neutral list.
        return `<section class="template-section">${title}<p class="muted template-note">No groups are declared for this section; items are shown in posting order.</p>${matching.map(post => renderPostCard(post, instance, true, component.field_ids)).join('') || '<div class="empty-state">No posts in this section.</div>'}</section>`;
      }
      // A further post_groups section is a work list of its own, from the rollup.
      const view = taskView(posts, board.board_id, component, instance, boardLinkContext.titles, new Set(posts.map(post => post.sequence)));
      return workPanelHtml(view, hasMorePosts, component.title || 'Work', `s${index}`);
    }
    const ordered = component.kind === 'post_timeline'
      ? matching.slice().sort((a, b) => String(a.created_at || '').localeCompare(String(b.created_at || '')))
      : matching;
    return `<section class="template-section${component.kind === 'post_timeline' ? ' template-section--timeline' : ''}">${title}${ordered.map(post => renderPostCard(post, instance, component.kind === 'post_timeline' ? 'timeline' : false)).join('') || '<div class="empty-state">No posts in this section.</div>'}</section>`;
  }
  if (component.kind === 'related_posts') {
    const allowedIds = component.post_template_ids;
    const refs = (board.links || []).filter(link => {
      const target = String(link.target || '');
      if (!target.includes(':post:') || (component.relation && link.relation !== component.relation)) return false;
      if (!Array.isArray(allowedIds) || allowedIds.length === 0) return true;
      const linkedPost = posts.find(post => post.post_id === target);
      return linkedPost && allowedIds.includes(linkedPost.post_template_id);
    });
    return `<section class="template-section">${title}<div class="template-reference-grid template-reference-grid--${component.mode === 'compact' ? 'list' : 'cards'}">${renderBoardLinks(refs)}</div></section>`;
  }
  if (component.kind === 'related_boards') {
    const items = await linkedBoardCards(board, component, used);
    return `<section class="template-section">${title}<div class="template-reference-grid template-reference-grid--${component.mode === 'list' ? 'list' : 'cards'}">${items}</div></section>`;
  }
  // Unknown kinds come from newer template versions. Show their declared props
  // as escaped text so nothing is hidden and nothing is interpreted.
  let props = '';
  try { props = JSON.stringify(component, null, 2).slice(0, 4000); } catch { props = ''; }
  return `<section class="template-section template-section--unsupported">${title}<details data-detail-id="component-${index}"><summary><span class="muted">This dashboard can't show a <code>${esc(component.kind)}</code> section yet.</span> View its settings</summary><pre>${esc(props)}</pre></details></section>`;
}

function templateProvenanceHtml(instance) {
  const postTemplates = Array.isArray(instance.snapshot?.post_templates) ? instance.snapshot.post_templates : [];
  return `<details class="board-template-label" data-detail-id="template-provenance">
    <summary>Template · ${esc(instance.issuer || 'unknown issuer')} / ${esc(instance.template_id || 'unknown')} v${esc(instance.version || 'unknown')}</summary>
    <dl class="template-fields">
      <div><dt>Content hash</dt><dd class="mono">${esc(instance.content_hash || 'unavailable')}</dd></div>
      <div><dt>Contract</dt><dd>v${esc(instance.contract_version || '—')}</dd></div>
      <div><dt>Post templates</dt><dd>${postTemplates.length ? postTemplates.map(item => `${esc(item.template_id)} v${esc(item.version)}`).join(', ') : 'None'}</dd></div>
    </dl>
    <p class="muted">The issuer is recorded provenance, not a verified signature.</p>
  </details>`;
}

/* ── board post presentation ───────────────────────────────────── */

const expandedPosts = new Set();
const boardMeta = new Map();
const RELATIVE_UNITS = [['year', 31536000], ['month', 2592000], ['week', 604800], ['day', 86400], ['hour', 3600], ['minute', 60]];
const relativeFormat = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto', style: 'short' });
const absoluteTime = value => value ? new Date(value).toLocaleString() : 'Time unavailable';

function relativeTime(value) {
  const time = new Date(value).getTime();
  if (!Number.isFinite(time)) return 'time unavailable';
  const delta = (time - Date.now()) / 1000;
  for (const [unit, size] of RELATIVE_UNITS) if (Math.abs(delta) >= size) return relativeFormat.format(Math.round(delta / size), unit);
  return 'just now';
}

function timeHtml(value) {
  return value ? `<time datetime="${esc(value)}" title="${esc(absoluteTime(value))}">${esc(relativeTime(value))}</time>` : '<time>time unavailable</time>';
}

// An agent's chip color is a stable hash of its ID: identity, not status.
function agentTone(name) {
  let hash = 0;
  for (const char of String(name)) hash = (hash * 31 + char.codePointAt(0)) >>> 0;
  return hash % 6;
}

function agentChip(name) {
  const label = String(name || 'agent');
  const initial = label.replace(/^[^a-z0-9]+/i, '').charAt(0).toUpperCase() || '?';
  return `<span class="agent-chip agent-tone--${agentTone(label)}" title="${esc(label)}"><i aria-hidden="true">${esc(initial)}</i>${esc(label)}</span>`;
}

// Escaped first; then only `code`, **bold** and a leading "Label:" are marked up.
function inlineText(line) {
  return esc(line)
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/^([A-Z][A-Za-z0-9 /&-]{1,28}):(\s)/, '<span class="rich-label">$1:</span>$2');
}

// Post text is plain text with light Markdown habits. Lines are grouped into
// headings, lists, code blocks and paragraphs; nothing is interpreted as HTML.
function richTextHtml(text) {
  const out = [];
  let paragraph = [];
  let list = null;
  let code = null;
  const flushParagraph = () => {
    if (paragraph.length) out.push(`<p>${paragraph.map(inlineText).join('<br>')}</p>`);
    paragraph = [];
  };
  const flushList = () => {
    if (list) out.push(`<${list.tag}>${list.items.map(item => `<li${item.nested ? ' class="is-nested"' : ''}${item.value ? ` value="${item.value}"` : ''}>${inlineText(item.text)}</li>`).join('')}</${list.tag}>`);
    list = null;
  };
  for (const raw of String(text || '').replace(/\r\n?/g, '\n').split('\n')) {
    if (code) {
      if (/^\s*```/.test(raw)) { out.push(`<pre><code>${esc(code.join('\n'))}</code></pre>`); code = null; } else code.push(raw);
      continue;
    }
    if (/^\s*```/.test(raw)) { flushParagraph(); flushList(); code = []; continue; }
    const line = raw.trimEnd();
    if (!line.trim()) { flushParagraph(); flushList(); continue; }
    const heading = line.match(/^(#{1,4})\s+(.+)$/);
    if (heading) { flushParagraph(); flushList(); out.push(`<h4 class="rich-heading">${inlineText(heading[2])}</h4>`); continue; }
    const bullet = line.match(/^(\s*)[-*•]\s+(.+)$/);
    const ordered = line.match(/^(\s*)(\d{1,3})[.)]\s+(.+)$/);
    if (bullet || ordered) {
      flushParagraph();
      const tag = bullet ? 'ul' : 'ol';
      const indent = (bullet || ordered)[1].length;
      // Indented items stay inside the current list as sub-points.
      if (!list || (list.tag !== tag && indent === 0)) { flushList(); list = { tag, items: [] }; }
      list.items.push({ text: bullet ? bullet[2] : ordered[3], nested: indent > 0, value: ordered && list.tag === 'ol' ? Number(ordered[2]) : null });
      continue;
    }
    flushList();
    paragraph.push(line);
  }
  if (code) out.push(`<pre><code>${esc(code.join('\n'))}</code></pre>`);
  flushParagraph();
  flushList();
  return out.join('');
}

// A post without a title uses its first line when that line reads as one.
function postTitleParts(post) {
  const content = String(post.content || '');
  if (post.title) return { title: post.title, body: content };
  const newline = content.indexOf('\n');
  const first = (newline < 0 ? content : content.slice(0, newline)).trim().replace(/^#{1,4}\s+/, '');
  if (first && first.length <= 140) return { title: first, body: newline < 0 ? '' : content.slice(newline + 1) };
  // A long opening line is shortened at a word boundary for the title, and
  // the body keeps the full text.
  if (first) {
    const cut = first.slice(0, 100);
    return { title: `${cut.slice(0, Math.max(cut.lastIndexOf(' '), 60)).replace(/[\s,;:·—-]+$/, '')}…`, body: content };
  }
  return { title: null, body: content };
}

const FOLD_CHARS = 1100;
const FOLD_LINES = 18;

function postBodyHtml(post, body) {
  if (!body.trim()) return '';
  const lines = body.split('\n').length;
  const foldable = body.length > FOLD_CHARS || lines > FOLD_LINES;
  const folded = foldable && !expandedPosts.has(post.post_id);
  return `<div class="board-post__body${folded ? ' is-folded' : ''}" data-fold-body>
      <div class="rich-text">${richTextHtml(body)}</div>
    </div>
    ${foldable ? `<button type="button" class="board-post__fold" data-fold-post="${esc(post.post_id)}" data-fold-lines="${lines}" aria-expanded="${folded ? 'false' : 'true'}">${folded ? `Show full post · ${fmt(lines)} lines` : 'Show less'}</button>` : ''}`;
}

function bindPostFolds(container) {
  container.querySelectorAll('[data-fold-post]').forEach(button => button.addEventListener('click', () => {
    const body = button.previousElementSibling;
    const expand = !expandedPosts.has(button.dataset.foldPost);
    if (expand) expandedPosts.add(button.dataset.foldPost); else expandedPosts.delete(button.dataset.foldPost);
    body?.classList.toggle('is-folded', !expand);
    button.setAttribute('aria-expanded', String(expand));
    button.textContent = expand ? 'Show less' : `Show full post · ${fmt(button.dataset.foldLines)} lines`;
    if (!expand) button.closest('.board-post')?.scrollIntoView({ block: 'nearest' });
  }));
}

// Sidebar summary per board: post count and the time of its newest post. It
// reads at most three pages so a very long board is reported as "300+".
async function loadBoardMeta(boardId) {
  const cached = boardMeta.get(boardId);
  if (cached && (cached.pending || Date.now() - cached.at < 60000)) return;
  boardMeta.set(boardId, { ...cached, pending: true });
  try {
    let count = 0;
    let last = null;
    let after = null;
    let more = false;
    for (let page = 0; page < 3; page += 1) {
      const result = await api(`/api/boards/${encodeURIComponent(boardId)}?limit=100${after != null ? `&after_sequence=${after}` : ''}`);
      const posts = result.posts || [];
      count += posts.length;
      if (posts.length) last = posts.at(-1).created_at || last;
      more = Boolean(result.has_more);
      after = result.next_after_sequence ?? lastPostSequence(posts);
      if (!more) break;
    }
    boardMeta.set(boardId, { count, last: more ? null : last, more, at: Date.now() });
  } catch {
    boardMeta.set(boardId, { ...cached, pending: false, at: Date.now() });
  }
  updateBoardMetaLabel(boardId);
}

function boardMetaText(boardId) {
  const meta = boardMeta.get(boardId);
  if (!meta || meta.count == null) return '';
  const count = `${fmt(meta.count)}${meta.more ? '+' : ''} ${meta.count === 1 && !meta.more ? 'post' : 'posts'}`;
  return meta.last ? `${count} · ${relativeTime(meta.last)}` : count;
}

function updateBoardMetaLabel(boardId) {
  const label = document.querySelector(`#boards-list [data-board-meta="${CSS.escape(boardId)}"]`);
  if (label) label.textContent = boardMetaText(boardId);
}

// Keep the sidebar summary of the open board in step with what is on screen.
function noteBoardSnapshotMeta(snapshot) {
  if (snapshot.hasMore) return;
  const posts = snapshot.posts || [];
  boardMeta.set(snapshot.board.board_id, { count: posts.length, last: posts.at(-1)?.created_at || null, more: false, at: Date.now() });
  updateBoardMetaLabel(snapshot.board.board_id);
}

function contributorsHtml(posts) {
  const counts = new Map();
  for (const post of posts) {
    const name = post.author_agent_id || post.provider || 'agent';
    counts.set(name, (counts.get(name) || 0) + 1);
  }
  const ranked = [...counts.entries()].sort((a, b) => b[1] - a[1]);
  if (!ranked.length) return '';
  const shown = ranked.slice(0, 8);
  return `<ul class="board-side__contributors">${shown.map(([name, count]) => `<li>${agentChip(name)}<b>${fmt(count)} ${count === 1 ? 'post' : 'posts'}</b></li>`).join('')}</ul>${ranked.length > shown.length ? `<p class="muted board-side__more">+${fmt(ranked.length - shown.length)} more</p>` : ''}`;
}

/* ── board view: tiles, work rows, side panel ──────────────────── */

// Order of the work groups: the board's declared post_groups order, then
// Other, then the settled groups (a declared success tone, or the common done
// and cancelled values) in their declared order. Settled groups go last so
// they fold away at the end of the list and sit at the right in Columns.
// Nothing else is reordered. Tiles, List, Columns and the Flow page all use
// this order.
const SETTLED_STATUSES = new Set(['done', 'cancelled']);

function workGroups(rollup, component) {
  const declared = Array.isArray(component.groups)
    ? component.groups.filter(group => group && typeof group === 'object' && Object.hasOwn(group, 'value')) : [];
  const groups = rollup.groups.map((group, index) => {
    const tone = GROUP_TONES.has(declared[index]?.tone) ? declared[index].tone : `n${index % 6}`;
    const settled = tone === 'success' || (!GROUP_TONES.has(declared[index]?.tone) && SETTLED_STATUSES.has(group.value));
    return { ...group, tone, settled };
  });
  const other = rollup.other.count ? [{ ...rollup.other, tone: 'other', settled: false }] : [];
  return [...groups.filter(group => !group.settled), ...other, ...groups.filter(group => group.settled)];
}

// Everything the work list needs for one post_groups section, built from the
// task rollup. `anchored` collects the post sequences that already have an
// on-page anchor, so each post is anchored once.
function taskView(posts, boardId, component, instance, titles, anchored) {
  const rollup = rollupTasks(posts, boardId, component);
  const groups = workGroups(rollup, component);
  const byValue = new Map(groups.filter(group => group.value !== null).map(group => [group.value, group]));
  return {
    boardId, instance, rollup, groups, titles, anchored, statusField: rollup.statusField,
    postsBySeq: new Map(posts.map(post => [post.sequence, post])),
    labelOf: value => byValue.get(value)?.label ?? String(value),
    toneOf: value => byValue.get(value)?.tone || 'other',
  };
}

function statusPill(label, tone) {
  return `<span class="board-pill"><i class="group-tone--${esc(tone)}" aria-hidden="true"></i>${esc(label)}</span>`;
}

// One post in a task's history: its own status, author, time, full text,
// fields and labelled links.
function historyEntryHtml(post, view) {
  const anchor = !view.anchored.has(post.sequence);
  if (anchor) view.anchored.add(post.sequence);
  const { title, body } = postTitleParts(post);
  const fields = post.fields && typeof post.fields === 'object' && !Array.isArray(post.fields) ? post.fields : {};
  const schema = postTemplateFor(view.instance, post)?.schema;
  const status = fields[view.statusField];
  const ids = [...new Set([...fieldIndex(schema).keys(), ...Object.keys(fields)])].filter(id => id !== view.statusField);
  const rows = fieldRows(fields, schema, ids);
  return `<article class="board-entry"${anchor ? ` id="post-${esc(post.sequence)}"` : ''}>
      <header class="board-entry__head">
        <span class="board-entry__seq">#${esc(post.sequence)}</span>
        <strong class="board-entry__title">${esc(title || `Post ${post.sequence}`)}</strong>
        <span class="board-entry__meta">${isEmptyValue(status) ? '' : statusPill(view.labelOf(status), view.toneOf(status))}${agentChip(post.author_agent_id || post.provider || 'agent')}${timeHtml(post.created_at)}</span>
      </header>
      ${postBodyHtml(post, body)}
      ${rows ? `<dl class="template-fields board-entry__fields">${rows}</dl>` : ''}
      ${(post.links || []).length ? renderBoardLinks(post.links) : ''}
    </article>`;
}

// The disclosure ID of a task row. The main panel keeps the plain form; a
// further post_groups section prefixes its scope so IDs stay unique.
function taskDetailId(sequence, scope) {
  return scope === 'main' ? `task-${sequence}` : `${scope}-task-${sequence}`;
}

// The summary parts shared by List rows and Columns cards.
function taskSummaryHtml(task, view) {
  const updates = task.updates.length;
  const latest = updates ? `<span class="board-task__latest-title" title="${esc(task.latest.title)}">${esc(task.latest.title)}</span>` : '<span class="muted">No updates yet</span>';
  return `<span class="board-task__title" title="${esc(task.title)}">${esc(task.title)}</span>
        ${statusPill(task.statusLabel, view.toneOf(task.status))}
        <span class="board-task__assignee">${task.assignee ? agentChip(task.assignee) : '<span class="muted">Unassigned</span>'}</span>
        <span class="board-task__latest">${latest}${timeHtml(task.latest.createdAt)}</span>
        <span class="board-task__count">${fmt(updates)} ${updates === 1 ? 'update' : 'updates'}</span>`;
}

// A task's history: what it depends on, then the root post and its updates
// in posting order. Shared by List rows and the Columns detail area.
function taskHistoryHtml(task, view) {
  const members = [task.sequence, ...task.updates].map(sequence => view.postsBySeq.get(sequence)).filter(Boolean);
  const depends = task.dependsOn.length
    ? `<p class="board-task__depends"><span class="muted">Depends on</span>${task.dependsOn.map(sequence => `<button type="button" class="board-link" data-board-link="${esc(view.boardId)}" data-post-sequence="${esc(sequence)}">${esc(view.titles.get(sequence) || `post ${sequence}`)}</button>`).join('')}</p>` : '';
  return `<div class="board-task__history">${depends}${members.map(post => historyEntryHtml(post, view)).join('')}</div>`;
}

// A List row: compact summary, expanding in place to the task's history.
function taskRowHtml(task, view, scope) {
  return `<details class="board-task" data-detail-id="${esc(taskDetailId(task.sequence, scope))}">
      <summary class="board-task__summary">
        ${taskSummaryHtml(task, view)}
      </summary>
      ${taskHistoryHtml(task, view)}
    </details>`;
}

// A Columns card: the summary only, never expanding in the column. A button,
// so click, Enter and Space all open the task's details under the columns.
function taskCardHtml(task, view, scope, selected) {
  return `<button type="button" class="board-task board-task--card${selected ? ' is-selected' : ''}" data-task-open="${esc(task.sequence)}" data-task-scope="${esc(scope)}" aria-expanded="${selected}" aria-controls="${esc(`${scope}-task-detail`)}" title="Show details below">
        ${taskSummaryHtml(task, view)}
      </button>`;
}

// The Columns detail area content for one task. Task posts are never anchored
// by Activity, so the main panel anchors their history afresh each time;
// other panels share the main panel's posts and anchor nothing.
function taskDetailHtml(task, view, scope) {
  const local = scope === 'main' ? { ...view, anchored: new Set() } : view;
  const updates = task.updates.length;
  return `<header class="board-task-detail__head">
      <h3 class="board-task-detail__title" data-task-detail-title tabindex="-1"><span class="board-entry__seq">#${esc(task.sequence)}</span>${esc(task.title)}</h3>
      <span class="board-task-detail__meta">${statusPill(task.statusLabel, view.toneOf(task.status))}${task.assignee ? agentChip(task.assignee) : '<span class="muted">Unassigned</span>'}<span class="board-task__count">${fmt(updates)} ${updates === 1 ? 'update' : 'updates'}</span></span>
      <button type="button" class="board-task-detail__close" data-task-close aria-label="${esc(`Close details for ${task.title}`)}" title="Close (Esc)">×</button>
    </header>
    ${taskHistoryHtml(task, local)}`;
}

function workPanelHtml(view, partial, eyebrow = 'Work', scope = 'main') {
  // workGroups already holds the declared order with settled groups last.
  const ordered = view.groups.filter(group => group.count);
  const byRecent = tasksByRecentActivity;
  const total = view.rollup.total;
  const mode = effectiveWorkMode();
  // An open task belongs to one board; another board starts with none open.
  if (workOpenBoardId !== view.boardId) {
    workOpenTask.clear();
    workOpenBoardId = view.boardId;
  }
  workViews.set(scope, view);
  // The switch shows only where Columns are possible; phones always get the List.
  const switchHtml = total ? `<div class="work-switch" role="group" aria-label="Work layout">${[['columns', 'Columns'], ['list', 'List']].map(([value, label]) => `<button type="button" data-work-mode="${value}" aria-pressed="${workMode === value}">${label}</button>`).join('')}</div>` : '';
  const body = !total ? '<div class="empty-state">No task posts on this board yet.</div>'
    : mode === 'columns' ? workColumnsHtml(ordered, view, byRecent, scope)
    : workListHtml(view, scope);
  return `<section class="panel board-work board-work--${mode}" data-work-scope="${esc(scope)}">
      <div class="panel__head"><div><p class="eyebrow">${esc(eyebrow)}</p><h2>${fmt(total)} ${total === 1 ? 'task' : 'tasks'}</h2></div><div class="board-work__tools"><span class="panel__note">${partial ? 'From the posts loaded so far' : 'In the board’s declared order'}</span>${switchHtml}</div></div>
      ${body}
    </section>`;
}

// Collapsed work groups, per board and per group, shared by Columns and List.
// Settled groups start collapsed and every other group starts expanded until
// the reader toggles it. The map is cached in memory, so the state holds even
// when browser storage is unavailable.
const workCollapseCache = new Map();

function workCollapseKey(boardId) {
  return `membrick-dashboard-work-collapsed:${boardId}`;
}

function workCollapseState(boardId) {
  if (!workCollapseCache.has(boardId)) {
    let saved = {};
    try {
      const parsed = JSON.parse(storageGet(workCollapseKey(boardId)) || '{}');
      if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) saved = parsed;
    } catch { /* unreadable state starts fresh */ }
    workCollapseCache.set(boardId, saved);
  }
  return workCollapseCache.get(boardId);
}

function workGroupId(group, scope) {
  return `${scope}:${group.value === null || group.value === undefined ? 'other' : `v:${String(group.value)}`}`;
}

function workGroupCollapsed(boardId, group, scope) {
  const saved = workCollapseState(boardId)[workGroupId(group, scope)];
  return typeof saved === 'boolean' ? saved : Boolean(group.settled);
}

function setWorkGroupCollapsed(boardId, groupId, collapsed) {
  const saved = workCollapseState(boardId);
  saved[groupId] = collapsed;
  storageSet(workCollapseKey(boardId), JSON.stringify(saved));
}

// Apply a collapse change to the groups on screen without a re-render.
function applyWorkGroupCollapsed(element, collapsed) {
  element.classList.toggle('is-collapsed', collapsed);
  const button = element.querySelector(':scope > .board-col__head > [data-work-collapse], :scope > .board-work__label > [data-work-collapse]');
  if (!button) return;
  button.setAttribute('aria-expanded', String(!collapsed));
  button.setAttribute('aria-label', `${collapsed ? 'Expand' : 'Collapse'} ${button.dataset.groupLabel}`);
  const body = element.querySelector(':scope > [data-work-body]');
  if (body) body.hidden = collapsed;
}

// The shared group toggle: a button holding the dot, label, count and a sign.
function workToggleHtml(group, view, scope, collapsed) {
  const label = `${group.label}, ${group.count} ${group.count === 1 ? 'task' : 'tasks'}`;
  return `<button type="button" class="work-toggle" data-work-collapse="${esc(workGroupId(group, scope))}" data-group-label="${esc(label)}" aria-expanded="${!collapsed}" aria-label="${esc(`${collapsed ? 'Expand' : 'Collapse'} ${label}`)}">
      <i class="group-dot group-tone--${esc(group.tone)}" aria-hidden="true"></i><span class="board-col__label">${esc(group.label)}</span><b>${fmt(group.count)}</b><span class="work-toggle__sign" aria-hidden="true"></span>
    </button>`;
}

// List: one list of every task, most recent activity first, so a task that
// just changed is at the top. Each row's pill shows its status.
function workListHtml(view, scope) {
  return `<div class="board-work__rows board-work__rows--recent">${tasksByRecentActivity(view.rollup.tasks).map(task => taskRowHtml(task, view, scope)).join('')}</div>`;
}

// Columns: one vertically scrolling column per group that has tasks, in the
// declared order and tones. A collapsed column is a narrow count column that
// expands on click. Cards hold summaries only. A card's details open in the
// area under the columns, so a column never changes width.
function workColumnsHtml(groups, view, byRecent, scope) {
  const openSequence = workOpenTask.get(scope);
  const openTask = openSequence == null ? null : view.rollup.tasks.find(task => task.sequence === openSequence) || null;
  if (openSequence != null && !openTask) workOpenTask.delete(scope);
  const columns = groups.map(group => {
    const id = workGroupId(group, scope);
    const collapsed = workGroupCollapsed(view.boardId, group, scope);
    const other = group.value === null || group.value === undefined;
    return `<section class="board-col${other ? ' board-col--other' : ''}${collapsed ? ' is-collapsed' : ''}" data-work-group="${esc(id)}" aria-label="${esc(`${group.label}, ${group.count} ${group.count === 1 ? 'task' : 'tasks'}`)}">
        <h3 class="board-col__head">${workToggleHtml(group, view, scope, collapsed)}</h3>
        <div class="board-col__cards" data-col-scroll="${esc(`work-col-${id}`)}" data-work-body${collapsed ? ' hidden' : ''}>${byRecent(group.tasks).map(task => taskCardHtml(task, view, scope, task === openTask)).join('')}</div>
      </section>`;
  });
  return `<div class="board-kanban">${columns.join('')}</div>
    <section class="board-task-detail" id="${esc(`${scope}-task-detail`)}" data-task-detail-area aria-label="Task details"${openTask ? '' : ' hidden'}>${openTask ? taskDetailHtml(openTask, view, scope) : ''}</section>`;
}

// One line per post, newest first. A post that belongs to a task jumps to it;
// any other post expands in place.
function activityPanelHtml(posts, view, more, postCount) {
  const items = posts.slice().reverse().map(post => {
    const { title, body } = postTitleParts(post);
    const links = Array.isArray(post.links) ? post.links : [];
    const first = links[0] ? parseLinkTarget(String(links[0].target || '')) : null;
    const relation = first && (first.kind === 'board' || first.sequence)
      ? `<span class="board-activity__rel">${esc(relationWords(links[0].relation))} ${esc(linkTargetLabel(first))}${links.length > 1 ? ` +${fmt(links.length - 1)}` : ''}</span>` : '';
    const line = `<span class="board-entry__seq">#${esc(post.sequence)}</span><span class="board-activity__title">${esc(title || `Post ${post.sequence}`)}</span>${relation}<span class="board-activity__meta">${agentChip(post.author_agent_id || post.provider || 'agent')}${timeHtml(post.created_at)}</span>`;
    if (view.rollup.rootsOf.has(post.sequence)) return `<li><button type="button" class="board-activity__line" data-post-focus="${esc(post.sequence)}">${line}</button></li>`;
    const anchor = !view.anchored.has(post.sequence);
    if (anchor) view.anchored.add(post.sequence);
    const rows = fieldRows(post.fields, postTemplateFor(view.instance, post)?.schema);
    return `<li><details class="board-activity__post"${anchor ? ` id="post-${esc(post.sequence)}"` : ''} data-detail-id="post-${esc(post.sequence)}"><summary class="board-activity__line">${line}</summary>
      <div class="board-activity__body">${postBodyHtml(post, body)}${rows ? `<dl class="template-fields board-entry__fields">${rows}</dl>` : ''}${links.length ? renderBoardLinks(links) : ''}</div></details></li>`;
  });
  return `<details class="panel board-activity" data-detail-id="activity">
      <summary class="panel__head"><div><p class="eyebrow">Activity</p><h2>${postCount} ${posts.length === 1 ? 'post' : 'posts'} · newest first</h2></div><span class="board-work__toggle">Show</span></summary>
      <ol class="board-activity__list">${items.join('') || '<li class="empty-state">No posts on this board yet.</li>'}</ol>${more}
    </details>`;
}

function boardTilesHtml(board, view, posts, postCount, partial) {
  const last = posts.at(-1);
  const lastTile = `<article class="kpi kpi--time"><span>Last activity</span><strong>${last ? esc(relativeTime(last.created_at)) : '—'}</strong><small>${last ? `#${esc(last.sequence)} · ${esc(last.author_agent_id || last.provider || 'agent')}` : 'No posts yet'}</small></article>`;
  if (view) {
    const tiles = view.groups.filter(group => group.count).map(group => `<article class="kpi"><span><i class="group-dot group-tone--${esc(group.tone)}" aria-hidden="true"></i>${esc(group.label)}</span><strong>${fmt(group.count)}</strong><small>${group.count === 1 ? 'task' : 'tasks'}${partial ? ' loaded' : ''}</small></article>`);
    return `<div class="kpis board-kpis">${tiles.join('')}${lastTile}</div>`;
  }
  const contributors = new Set(posts.map(post => post.author_agent_id || post.provider || 'agent')).size;
  return `<div class="kpis board-kpis">
      <article class="kpi"><span>Posts</span><strong>${postCount}</strong><small>${partial ? 'loaded so far' : 'on this board'}</small></article>
      <article class="kpi"><span>Contributors</span><strong>${fmt(contributors)}</strong><small>${contributors === 1 ? 'agent' : 'agents'} posting</small></article>
      ${lastTile}
      <article class="kpi kpi--time"><span>Created</span><strong>${board.created_at ? esc(relativeTime(board.created_at)) : '—'}</strong><small>${esc(absoluteTime(board.created_at))}</small></article>
    </div>`;
}

// Flows anchored on a board, read from the Flows API at most every 20s.
const boardFlowCache = { at: 0, flows: [], pending: null };

async function boardFlows(boardId) {
  if (!boardFlowCache.pending && Date.now() - boardFlowCache.at > 20000) {
    boardFlowCache.pending = api('/api/flows')
      .then(result => { boardFlowCache.flows = Array.isArray(result?.flows) ? result.flows : []; })
      .catch(() => {})
      .finally(() => { boardFlowCache.at = Date.now(); boardFlowCache.pending = null; });
  }
  const pending = boardFlowCache.pending;
  if (pending) await pending;
  return boardFlowCache.flows.map(item => item?.run).filter(run => run && run.board_id === boardId)
    .sort((a, b) => (a.state === 'completed') - (b.state === 'completed') || String(b.updated_at || '').localeCompare(String(a.updated_at || '')));
}

function boardFlowHtml(runs) {
  if (!runs.length) return '';
  const [run, ...others] = runs;
  const snapshot = run.flow_template?.snapshot || {};
  // Same stepper rules as the Flows page, including stage_completed events.
  const events = boardFlowCache.flows.find(item => item?.run?.flow_id === run.flow_id)?.events;
  const steps = flowStageSteps(snapshot.stages || [], run.stage, run.state, Array.isArray(events) ? events : [])
    .map(step => `<li class="board-flow__step is-${esc(step.state)}"${step.state === 'current' ? ' aria-current="step"' : ''}><i aria-hidden="true"></i><span>${esc(step.name)}</span></li>`).join('');
  const coordinator = run.coordinator?.agent_id;
  const flowHref = id => `#flows/${esc(encodeURIComponent(String(id || '')))}`;
  return `<section class="board-side__block board-flow">
      <div class="board-side__head"><p class="eyebrow">Flow</p><a class="board-link" href="${flowHref(run.flow_id)}">Open in Flows</a></div>
      <strong class="board-flow__name">${esc(snapshot.name || run.template_id || 'Flow')}</strong>
      ${run.goal ? `<p class="board-flow__goal" title="${esc(run.goal)}">${esc(run.goal)}</p>` : ''}
      ${steps ? `<ol class="board-flow__steps">${steps}</ol>` : `<p class="muted">Stage ${esc(run.stage || '—')}</p>`}
      <dl class="board-flow__meta">
        <div><dt>State</dt><dd>${esc(run.state || 'unknown')}</dd></div>
        <div><dt>Coordinator</dt><dd>${coordinator ? agentChip(coordinator) : '<span class="muted">None</span>'}</dd></div>
        <div><dt>Updated</dt><dd>${timeHtml(run.updated_at)}</dd></div>
      </dl>
      ${others.length ? `<details class="board-flow__others" data-detail-id="flow-others"><summary>${fmt(others.length)} other ${others.length === 1 ? 'flow' : 'flows'} on this board</summary>
        <ul>${others.map(other => `<li><a class="board-link" href="${flowHref(other.flow_id)}" title="${esc(other.goal || '')}">${esc(other.goal || snapshot.name || 'Flow')}</a><small>${esc(other.state || '')} · ${timeHtml(other.updated_at)}</small></li>`).join('')}</ul></details>` : ''}
    </section>`;
}

// The hero's summary fields (for example success criteria) as a collapsible
// side block. A single field shows its value under its own label.
function boardSummaryHtml(hero, fields, schema) {
  const ids = Array.isArray(hero?.summary_fields) ? hero.summary_fields.filter(id => typeof id === 'string' && Object.hasOwn(fields, id) && !isEmptyValue(fields[id])) : [];
  if (!ids.length) return '';
  const definitions = fieldIndex(schema);
  const label = ids.length === 1 ? definitions.get(ids[0])?.label || ids[0].replaceAll('_', ' ') : 'Summary';
  const content = ids.length === 1 ? `<div class="board-side__value">${fieldValueHtml(fields[ids[0]])}</div>` : `<dl class="template-fields board-side__fields">${fieldRows(fields, schema, ids)}</dl>`;
  return `<details class="board-side__block board-side__summary" data-detail-id="summary" open>
      <summary class="board-side__head"><p class="eyebrow">${esc(label)}</p><span class="board-work__toggle">Show</span></summary>
      ${content}
    </details>`;
}

async function renderBoardDetail(snapshot) {
  const renderId = ++state.boardRenderSeq;
  const generation = state.boardGeneration;
  const usedLinked = new Set();
  const board = snapshot.board;
  const instance = board.board_template || null;
  const templateSnapshot = instance?.snapshot || null;
  const loadedPosts = snapshot.posts || [];
  // Keep chronological history for pagination, while also showing the newest
  // page when older history has not been loaded yet. Merge by sequence so a
  // latest-page fetch never renders duplicates.
  const posts = mergeBoardPosts(loadedPosts, snapshot.latestPosts || []);
  const partial = Boolean(snapshot.hasMore);
  const components = Array.isArray(templateSnapshot?.presentation?.components) ? templateSnapshot.presentation.components : [];
  const hero = components.find(component => component?.kind === 'hero') || null;
  // The first post_groups section drives the tiles and the Work panel. Every
  // status and count shown there comes from the task rollup.
  const groupComponent = components.find(component => component?.kind === 'post_groups') || null;
  const titles = new Map(posts.map(post => [post.sequence, postTitleParts(post).title || `Post ${post.sequence}`]));
  boardLinkContext = { boardId: board.board_id, titles };
  const view = groupComponent ? taskView(posts, board.board_id, groupComponent, instance, titles, new Set()) : null;
  const links = renderBoardLinks(board.links || [], boardLinkContext, 6);
  const telemetryEvents = (state.status?.integrations || [])
    .flatMap(provider => (provider.events || []).map(event => ({ ...event, provider: provider.provider })));
  const postHtml = posts.map(post => {
    const postTemplate = postTemplateFor(instance, post);
    const structured = post.fields && typeof post.fields === 'object'
      ? `<section class="board-post__fields"><span class="eyebrow">Post template · ${esc(postTemplate?.template_id || post.post_template_id || 'structured post')} v${esc(post.post_template_version || '')}${post.post_template_id && !postTemplate ? ' · not in the pinned snapshot' : ''}</span><dl>${fieldRows(post.fields, postTemplate?.schema)}</dl></section>` : '';
    const correlatedEvents = post.session_key
      ? telemetryEvents.filter(event => event.session_key === post.session_key)
      : [];
    const telemetry = correlatedEvents.length ? `
      <section class="board-post__telemetry" aria-label="Observed session telemetry">
        <span class="eyebrow">Observed telemetry · ${fmt(correlatedEvents.length)} events · ${esc(post.session_key)}</span>
        <div>${correlatedEvents.map(event => `<span class="board-event">${esc(event.event || event.category || 'Event')} · ${esc(event.provider || 'provider')} · ${esc(event.timestamp_ms ? new Date(event.timestamp_ms).toLocaleString() : 'time unavailable')}</span>`).join('')}</div>
      </section>` : '';
    const { title, body } = postTitleParts(post);
    return `
    <article class="board-post" id="post-${esc(post.sequence)}">
      <span class="board-post__seq" aria-label="Post ${esc(post.sequence)}">${esc(post.sequence)}</span>
      <div class="board-post__card">
        <header class="board-post__head">
          <h3${title ? '' : ' class="is-untitled"'}>${esc(title || `Post ${post.sequence}`)}</h3>
          <div class="board-post__meta">${agentChip(post.author_agent_id || post.provider || 'agent')}${post.provider ? `<span class="provider-chip">${esc(post.provider)}</span>` : ''}${timeHtml(post.created_at)}</div>
        </header>
        ${postBodyHtml(post, body)}
        ${structured}
        ${(post.links || []).length ? `<div class="board-post__links"><span class="eyebrow">Post links</span>${renderBoardLinks(post.links)}</div>` : ''}
        ${telemetry}
      </div>
    </article>`;
  }).join('');
  const more = snapshot.hasMore
    ? `<div class="board-more"><button class="btn btn--quiet" type="button" id="board-more">Load next posts</button></div>` : '';
  const postCount = `${fmt(posts.length)}${partial ? '+' : ''}`;
  const postWord = posts.length === 1 && !partial ? 'post' : 'posts';
  const boardFields = board.fields && typeof board.fields === 'object' && !Array.isArray(board.fields) ? board.fields : {};
  const goal = hero?.title_field && Object.hasOwn(boardFields, hero.title_field) && !isEmptyValue(boardFields[hero.title_field])
    ? displayValue(boardFields[hero.title_field]) : '';
  // Everything that reads boardLinkContext is built before the first await.
  const workHtml = view ? workPanelHtml(view, partial) : '';
  const activityHtml = view ? activityPanelHtml(posts, view, more, postCount) : '';
  const tilesHtml = boardTilesHtml(board, view, posts, postCount, partial);
  const summaryHtml = boardSummaryHtml(hero, boardFields, templateSnapshot?.schema);
  const unknownFields = Object.keys(boardFields).filter(id => !fieldIndex(templateSnapshot?.schema).has(id));
  const unknownFieldRows = instance ? fieldRows(boardFields, templateSnapshot?.schema, unknownFields) : '';
  const fallbackBoardFields = !instance ? fieldRows(board.fields, null) : '';
  // Sections other than the hero and the main post_groups render as before.
  const extraComponents = components.filter(component => component !== hero && component !== groupComponent);
  const [sections, flows] = await Promise.all([
    Promise.all(extraComponents.map((component, index) => renderTemplateComponent(component, index, board, posts, instance, partial, usedLinked))),
    boardFlows(board.board_id),
  ]);
  // A newer render, or a different board selection, owns the panel now.
  if (renderId !== state.boardRenderSeq || generation !== state.boardGeneration || state.selectedBoardId !== board.board_id) return;
  for (const id of state.linkedBoards.keys()) if (!usedLinked.has(id)) state.linkedBoards.delete(id);
  const container = document.querySelector('#board-detail');
  const viewport = document.querySelector('.viewport');
  const sameBoard = container.dataset.boardId === board.board_id;
  const scrollTop = viewport.scrollTop;
  // Each disclosure keeps the state the reader left it in across live
  // re-renders. The map outlives the DOM, so a task opened in the List is
  // still open after a trip through Columns, which show no task rows.
  if (!sameBoard) boardDetailState.clear();
  else container.querySelectorAll('details[data-detail-id]').forEach(item => boardDetailState.set(item.dataset.detailId, item.open));
  const detailState = boardDetailState;
  boardTaskPosts = new Set(view ? view.rollup.rootsOf.keys() : []);
  // Columns keep their sideways and in-column scroll positions too.
  const kanbanScroll = sameBoard ? [...container.querySelectorAll('.board-kanban')].map(item => item.scrollLeft) : [];
  const columnScroll = sameBoard ? new Map([...container.querySelectorAll('[data-col-scroll]')].map(item => [item.dataset.colScroll, item.scrollTop])) : new Map();
  container.dataset.boardId = board.board_id;
  const lastActivity = posts.at(-1)?.created_at;
  const typeLabel = board.type && board.type !== 'general' ? `${board.type} board` : 'Board';
  const sectionHtml = sections.filter(Boolean).join('');
  const emptyHistory = '<div class="empty-state board-empty"><strong>No posts yet</strong><span>Agents add updates to this board with <code>board_post</code>. New posts appear here as they arrive.</span></div>';
  // Without a work list the full history is the main panel. When template
  // sections already summarize the posts, it starts folded.
  const historyHtml = view ? '' : sectionHtml
    ? `<details class="panel board-history" data-detail-id="history">
        <summary class="panel__head"><div><p class="eyebrow">All posts</p><h2>${postCount} ${postWord} · oldest first</h2></div><span class="board-work__toggle">Show</span></summary>
        <div class="board-timeline">${postHtml || emptyHistory}</div>${more}
      </details>`
    : `<section class="panel board-history">
        <div class="panel__head"><div><p class="eyebrow">Activity</p><h2>${postCount} ${postWord} · oldest first</h2></div>${posts.length > 3 ? '<button type="button" class="btn btn--quiet" id="board-jump-latest">Jump to latest ↓</button>' : ''}</div>
        <div class="board-timeline">${postHtml || emptyHistory}</div>${more}
      </section>`;
  const sideBlock = (title, body) => `<section class="board-side__block"><div class="board-side__head"><p class="eyebrow">${esc(title)}</p></div>${body}</section>`;
  const side = [
    boardFlowHtml(flows),
    summaryHtml,
    posts.length ? sideBlock('Contributors', contributorsHtml(posts)) : '',
    (board.links || []).length ? sideBlock('Board links', links) : '',
    unknownFieldRows ? sideBlock('Additional fields', `<p class="muted template-note">Not declared by the pinned template schema.</p><dl class="template-fields board-side__fields">${unknownFieldRows}</dl>`) : '',
    fallbackBoardFields ? sideBlock('Board fields', `<dl class="template-fields board-side__fields">${fallbackBoardFields}</dl>`) : '',
    instance ? `<section class="board-side__block">${templateProvenanceHtml(instance)}</section>` : '',
  ].filter(Boolean).join('');
  container.innerHTML = `
    ${renderBoardTrail(board)}
    <div class="board-view">
      <header class="board-head">
        <div class="board-head__title">
          <p class="eyebrow">${esc(typeLabel)} · <span class="mono">${esc(board.key)}</span></p>
          <h2>${esc(board.title || board.key)}</h2>
          ${goal ? `<p class="board-head__goal" title="${esc(goal)}">${esc(goal)}</p>` : ''}
          <div class="board-stats">
            <span><b>${postCount}</b> ${postWord}</span>
            ${snapshot.pagingAll ? `<span class="board-stats__paging" role="status">Loading all posts… ${fmt(lastPostSequence(loadedPosts))} so far</span>` : ''}
            ${lastActivity ? `<span>Last activity ${timeHtml(lastActivity)}</span>` : ''}
            <span>Created ${timeHtml(board.created_at)}</span>
          </div>
        </div>
        <div class="board-detail__badges"><span class="board-live" id="board-live" aria-live="polite">${boardLiveMarkerHtml(snapshot)}</span></div>
      </header>
      ${tilesHtml}
      <div class="board-columns${side ? '' : ' board-columns--single'}">
        <div class="board-main">${workHtml}${sectionHtml}${historyHtml}${activityHtml}</div>
        ${side ? `<aside class="panel board-side" aria-label="Board context">${side}</aside>` : ''}
      </div>
    </div>`;
  for (const [id, open] of detailState) {
    const item = container.querySelector(`details[data-detail-id="${CSS.escape(id)}"]`);
    if (item) item.open = open;
  }
  container.querySelectorAll('.board-kanban').forEach((item, index) => { if (kanbanScroll[index]) item.scrollLeft = kanbanScroll[index]; });
  container.querySelectorAll('[data-col-scroll]').forEach(item => {
    const top = columnScroll.get(item.dataset.colScroll);
    if (top) item.scrollTop = top;
  });
  if (sameBoard) viewport.scrollTop = scrollTop;
  container.querySelectorAll('[data-work-mode]').forEach(button => button.addEventListener('click', () => {
    if (button.dataset.workMode !== workMode) setWorkMode(button.dataset.workMode);
  }));
  // A card toggles its task's details under the columns. The mode and the
  // page stay put; a keyboard press (click detail 0) moves focus to the heading.
  container.querySelectorAll('[data-task-open]').forEach(button => button.addEventListener('click', event => {
    const scope = button.dataset.taskScope;
    const sequence = Number(button.dataset.taskOpen);
    if (workOpenTask.get(scope) === sequence) closeWorkTask(scope, { restoreFocus: event.detail === 0 });
    else openWorkTask(scope, sequence, { keyboard: event.detail === 0 });
  }));
  container.querySelectorAll('[data-task-detail-area]').forEach(area => area.addEventListener('click', event => {
    if (!event.target.closest('[data-task-close]')) return;
    const scope = area.closest('[data-work-scope]')?.dataset.workScope;
    if (scope) closeWorkTask(scope, { restoreFocus: true });
  }));
  container.querySelectorAll('[data-work-collapse]').forEach(button => button.addEventListener('click', () => {
    const group = button.closest('[data-work-group]');
    if (!group) return;
    const collapsed = !group.classList.contains('is-collapsed');
    setWorkGroupCollapsed(board.board_id, button.dataset.workCollapse, collapsed);
    applyWorkGroupCollapsed(group, collapsed);
  }));
  bindBoardLinkButtons(container);
  container.querySelectorAll('[data-post-focus]').forEach(button => button.addEventListener('click', () => focusBoardPost(Number(button.dataset.postFocus))));
  container.querySelectorAll('[data-trail-index]').forEach(button => button.addEventListener('click', () => {
    const index = Number(button.dataset.trailIndex);
    const target = state.boardTrail[index];
    if (!target) return;
    state.boardTrail = state.boardTrail.slice(0, index);
    selectBoard(target.board_id);
  }));
  bindBoardLiveMarker(snapshot);
  bindPostFolds(container);
  noteBoardSnapshotMeta(snapshot);
  container.querySelector('#board-jump-latest')?.addEventListener('click', async () => {
    const latestSequence = posts.at(-1)?.sequence;
    if (latestSequence != null && await focusBoardPost(latestSequence)) return;
    container.querySelector('.board-timeline > .board-post:last-child')?.scrollIntoView({ behavior: stillness.matches ? 'auto' : 'smooth', block: 'start' });
  });
  container.querySelector('#board-more')?.addEventListener('click', () => loadBoardPosts(board.board_id, snapshot.nextAfter, true));
}

/* ── query activity: small multiples, one scale per measure ────── */

const SPARKS = {
  requests_per_second: { color: '--series-requests', label: 'Requests / sec', format: value => value.toFixed(2) },
  p95_ms: { color: '--series-latency', label: 'p95 latency', format: value => `${value.toFixed(1)} ms` },
  errors: { color: '--series-errors', label: 'Errors', format: value => fmt(Math.round(value)) }
};

function drawSparks() {
  const surface = cssVar('--surface-2');
  const grid = cssVar('--line');
  document.querySelectorAll('#sparks .spark').forEach(figure => {
    const spec = SPARKS[figure.dataset.metric];
    const values = state.series.map(point => Number(point[figure.dataset.metric] || 0));
    const index = state.hover != null && state.hover < values.length ? state.hover : values.length - 1;
    figure.querySelector('figcaption b').textContent = values.length ? spec.format(values[index]) : '—';
    const canvas = figure.querySelector('canvas');
    const end = drawSpark(canvas, values, cssVar(spec.color), surface, grid, state.hover);
    // The pulse marker is a DOM element so it can breathe in CSS rather than
    // re-rasterising three canvases every frame. Offset by the canvas's own
    // position in the figure, since the caption sits above it.
    figure.classList.toggle('has-pulse', Boolean(end) && state.hover == null);
    if (end) {
      figure.style.setProperty('--px', `${end.x + canvas.offsetLeft}px`);
      figure.style.setProperty('--py', `${end.y + canvas.offsetTop}px`);
    }
  });
  renderSparkTip();
}

// Quadratic smoothing through segment midpoints: rounder than a polyline and,
// unlike a cubic spline, it never overshoots outside the data's own range.
function tracePath(ctx, points) {
  ctx.moveTo(points[0].x, points[0].y);
  if (points.length === 1) return;
  for (let index = 0; index < points.length - 1; index++) {
    const current = points[index];
    const next = points[index + 1];
    ctx.quadraticCurveTo(current.x, current.y, (current.x + next.x) / 2, (current.y + next.y) / 2);
  }
  ctx.lineTo(points.at(-1).x, points.at(-1).y);
}

function drawSpark(canvas, values, color, surface, grid, hover) {
  const ratio = window.devicePixelRatio || 1;
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  if (!width || !height) return null;
  canvas.width = width * ratio;
  canvas.height = height * ratio;
  const ctx = canvas.getContext('2d');
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  ctx.clearRect(0, 0, width, height);

  // Inset so the 8px end marker is never clipped by the canvas edge.
  const pad = 5;
  const top = 9;
  const bottom = height - 5;
  const plot = width - pad * 2;
  // Zero baseline: every measure here is a non-negative rate or count.
  const max = Math.max(1e-9, ...values);
  const x = index => values.length === 1 ? width / 2 : pad + index / (values.length - 1) * plot;
  const y = value => bottom - (value / max) * (bottom - top);

  ctx.strokeStyle = grid;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, bottom + 0.5);
  ctx.lineTo(width, bottom + 0.5);
  ctx.stroke();

  if (!values.length) return null;
  const points = values.map((value, index) => ({ x: x(index), y: y(value) }));

  const gradient = ctx.createLinearGradient(0, top, 0, bottom);
  gradient.addColorStop(0, `${color}59`);
  gradient.addColorStop(1, `${color}00`);
  ctx.beginPath();
  tracePath(ctx, points);
  ctx.lineTo(points.at(-1).x, bottom);
  ctx.lineTo(points[0].x, bottom);
  ctx.closePath();
  ctx.fillStyle = gradient;
  ctx.fill();

  ctx.beginPath();
  tracePath(ctx, points);
  ctx.strokeStyle = color;
  ctx.lineWidth = 2;
  ctx.lineJoin = 'round';
  ctx.lineCap = 'round';
  ctx.stroke();

  const marker = hover != null && hover < values.length ? hover : values.length - 1;
  if (hover != null && hover < values.length) {
    ctx.strokeStyle = grid;
    ctx.lineWidth = 1;
    ctx.setLineDash([2, 3]);
    ctx.beginPath();
    ctx.moveTo(Math.round(x(hover)) + 0.5, top - 6);
    ctx.lineTo(Math.round(x(hover)) + 0.5, bottom);
    ctx.stroke();
    ctx.setLineDash([]);
  }
  ctx.beginPath();
  ctx.arc(points[marker].x, points[marker].y, 4, 0, Math.PI * 2);
  ctx.fillStyle = color;
  ctx.fill();
  // A 2px surface ring keeps the marker legible where it sits on the line.
  ctx.strokeStyle = surface;
  ctx.lineWidth = 2;
  ctx.stroke();
  return points[marker];
}

function renderSparkTip() {
  const tip = document.querySelector('#spark-tip');
  const point = state.hover != null ? state.series[state.hover] : null;
  if (!point) { tip.hidden = true; return; }
  tip.hidden = false;
  tip.innerHTML = `<strong>${clock(point.start_ms)}</strong>` + Object.entries(SPARKS)
    .map(([metric, spec]) => `<span><i style="background:${cssVar(spec.color)}"></i>${spec.label}<b>${spec.format(Number(point[metric] || 0))}</b></span>`)
    .join('');
  const canvas = sparks.querySelector('canvas');
  const ratio = state.series.length > 1 ? state.hover / (state.series.length - 1) : 0.5;
  tip.style.left = `${Math.round(ratio * canvas.clientWidth)}px`;
}

const sparks = document.querySelector('#sparks');
sparks.addEventListener('mousemove', event => {
  const rect = sparks.querySelector('canvas').getBoundingClientRect();
  const count = state.series.length;
  if (!count || !rect.width) return;
  const position = Math.round((event.clientX - rect.left) / rect.width * (count - 1));
  const next = Math.min(count - 1, Math.max(0, position));
  if (next !== state.hover) { state.hover = next; drawSparks(); }
});
sparks.addEventListener('mouseleave', () => { state.hover = null; drawSparks(); });

/* ── index ─────────────────────────────────────────────────────── */

// The project view spans the shared workspace corpus and every discovered
// provider store. Keep segments qualified by their store name so identical
// local segment ids from separate indexes never look like one segment.
function projectIndexView(primary, integrations) {
  const stores = [];
  if (primary?.snapshot) stores.push({
    name: 'workspace-memory',
    snapshot: primary.snapshot,
    snapshot_revision: primary.snapshot_revision,
  });
  integrations.flatMap(provider => provider.indexes || []).forEach(index => {
    if (index.snapshot) stores.push({
      name: index.name,
      snapshot: index.snapshot,
      snapshot_revision: index.snapshot_revision,
    });
  });
  if (!stores.length) return primary;
  const segments = stores.flatMap(store => (store.snapshot.segments || []).map(segment => ({
    ...segment,
    index_name: store.name,
    segment_id: `${store.name}/${segment.segment_id}`
  })));
  return {
    ...primary,
    snapshot_revision: Math.max(...stores.map(store => Number(store.snapshot_revision || 0))),
    snapshot: {
      layout: 'project stores',
      store_count: stores.length,
      segment_count: segments.length,
      global_document_count: segments.reduce((sum, segment) => sum + Number(segment.document_count || 0), 0),
      segments
    }
  };
}

// Sequential = one hue, monotonic lightness. On a dark ground "more" reads as
// brighter, so the ramp runs dim -> bright as document load rises.
const LOAD_RAMP = ['#1c5cab', '#256abf', '#2a78d6', '#3987e5', '#5598e7', '#6da7ec', '#86b6ef'];
const loadColor = fraction => LOAD_RAMP[Math.min(LOAD_RAMP.length - 1, Math.max(0, Math.round(fraction * (LOAD_RAMP.length - 1))))];

function segmentStats(segments) {
  const counts = segments.map(segment => Number(segment.document_count || 0));
  const total = counts.reduce((sum, count) => sum + count, 0);
  const average = counts.length ? total / counts.length : 0;
  const smallest = counts.length ? Math.min(...counts) : 0;
  const largest = counts.length ? Math.max(...counts) : 0;
  return { total, average, smallest, largest, spread: average ? (largest - smallest) / average : 0 };
}

function renderIndexBrief(index) {
  const target = document.querySelector('#index-brief');
  const snapshot = index?.snapshot;
  if (!snapshot) {
    target.innerHTML = '<div class="empty-state">No published segment snapshot yet.</div>';
    return;
  }
  const segments = snapshot.segments || [];
  const { total, largest, spread } = segmentStats(segments);
  const bars = segments.slice(0, 6).map((segment, position) => {
    const count = Number(segment.document_count || 0);
    const fraction = largest ? count / largest : 0;
    return `<div class="brief-bar"><span>${String(position + 1).padStart(2, '0')}</span><i style="width:${Math.max(3, fraction * 100)}%;background:${loadColor(fraction)}"></i><b>${fmt(count)}</b></div>`;
  }).join('');
  target.innerHTML = `
    <div class="brief-figures">
      <div><span>Segments</span><strong>${fmt(snapshot.segment_count)}</strong></div>
      <div><span>Documents</span><strong>${fmt(total)}</strong></div>
      <div><span>Revision</span><strong>${fmt(index.snapshot_revision)}</strong></div>
      <div><span>Balance</span><strong>${spread < 0.2 ? 'Even' : 'Uneven'}</strong></div>
    </div>
    <div>
      <p class="eyebrow">Document load · ${esc(snapshot.layout)}</p>
      <div class="brief-bars">${bars || '<div class="empty-state">No segments published.</div>'}</div>
      ${segments.length > 6 ? `<p class="panel__note" style="text-align:left;margin-top:6px">+${fmt(segments.length - 6)} more segments</p>` : ''}
    </div>`;
}

// Hub-and-spoke network diagram. Segments fan out on one or two arcs to the
// right of the router, so the trunk from the gateway stays clear on the left.
// Node radius and spoke weight both encode document load.
function topologyDiagram(snapshot, segments, largest) {
  const width = 360;
  const height = 340;
  const gateway = { x: 46, y: height / 2 };
  const hub = { x: 168, y: height / 2 };
  // A 118° fan reads as "routes out to the right" without wrapping back around
  // the hub and colliding with its caption.
  const arc = (118 * Math.PI) / 180;
  const twoRings = segments.length > 12;
  const count = segments.length;

  const nodes = segments.map((segment, index) => {
    const documents = Number(segment.document_count || 0);
    const fraction = largest ? documents / largest : 0;
    const position = count === 1 ? 0.5 : index / (count - 1);
    const angle = -arc / 2 + position * arc;
    const radius = twoRings ? (index % 2 ? 156 : 104) : 132;
    return {
      index, segment, documents, fraction, angle,
      x: hub.x + Math.cos(angle) * radius,
      y: hub.y + Math.sin(angle) * radius,
      r: 7.5 + fraction * 7,
      color: loadColor(fraction),
      // Brighter ramp steps carry dark type; dim steps carry light type.
      ink: fraction > 0.45 ? '#08111f' : '#dbe6f7'
    };
  });

  const spokes = nodes.map(node => {
    // Bow each spoke slightly so overlapping runs stay tellable apart.
    const midX = (hub.x + node.x) / 2 + Math.cos(node.angle) * 5;
    const midY = (hub.y + node.y) / 2 + Math.sin(node.angle) * 5 - 8;
    return `<path class="tspoke" id="tspoke-${node.index}" d="M${hub.x} ${hub.y} Q${midX.toFixed(1)} ${midY.toFixed(1)} ${node.x.toFixed(1)} ${node.y.toFixed(1)}" stroke-width="${(0.8 + node.fraction * 2).toFixed(2)}" style="--spoke:${node.color}"/>`;
  }).join('');

  const marks = nodes.map(node => `<g class="tnode" data-index="${node.index}" transform="translate(${node.x.toFixed(1)} ${node.y.toFixed(1)})"><title>Segment ${String(node.index + 1).padStart(2, '0')} · ${esc(node.segment.segment_id)} · ${fmt(node.documents)} documents</title><circle class="tnode__halo" r="${(node.r + 5).toFixed(1)}" style="--node:${node.color}"/><circle class="tnode__dot" r="${node.r.toFixed(1)}" style="--node:${node.color}"/><text class="tnode__label" dy="0.32em" fill="${node.ink}">${String(node.index + 1).padStart(2, '0')}</text></g>`).join('');

  // SMIL packets are emitted only when motion is welcome.
  const packets = stillness.matches ? '' : [0, 1.3, 2.6].map(delay =>
    `<circle class="tpacket" r="2.6"><animateMotion dur="3.9s" begin="${delay}s" repeatCount="indefinite" path="M${gateway.x + 30} ${gateway.y} L${hub.x - 27} ${hub.y}"/></circle>`).join('');

  const guides = (twoRings ? [104, 156] : [132]).map(radius =>
    `<path class="tguide" d="M${(hub.x + Math.cos(-arc / 2) * radius).toFixed(1)} ${(hub.y + Math.sin(-arc / 2) * radius).toFixed(1)} A${radius} ${radius} 0 0 1 ${(hub.x + Math.cos(arc / 2) * radius).toFixed(1)} ${(hub.y + Math.sin(arc / 2) * radius).toFixed(1)}"/>`).join('');

  return `<svg class="topology-svg" viewBox="0 0 ${width} ${height}" role="img" preserveAspectRatio="xMidYMid meet" aria-label="Query gateway feeds a segment router that fans out to ${fmt(count)} published segments, sized by document load">
    <g class="tguides">${guides}</g>
    <line class="ttrunk" x1="${gateway.x + 30}" y1="${gateway.y}" x2="${hub.x - 27}" y2="${hub.y}"/>
    ${packets}
    <g class="tspokes">${spokes}</g>
    <g class="tgateway" transform="translate(${gateway.x} ${gateway.y})">
      <rect x="-30" y="-24" width="60" height="48" rx="11"/>
      <path class="tglyph" d="M-9 -6 h18 M-9 0 h18 M-9 6 h12"/>
      <text class="tlabel" y="38">Query gateway</text>
      <text class="tsub" y="50">HTTP search</text>
    </g>
    <g class="thub" transform="translate(${hub.x} ${hub.y})">
      <circle class="thub__ring" r="25"/>
      <circle class="thub__core" r="14"/>
      <text class="tlabel" y="-36">Segment router</text>
      <text class="tsub" y="-25">${esc(snapshot.layout)} layout</text>
    </g>
    <g class="tmarks">${marks}</g>
  </svg>`;
}

function renderIndexTopology(index) {
  const target = document.querySelector('#index-topology');
  const snapshot = index?.snapshot;
  if (!snapshot) {
    target.innerHTML = '<section class="panel"><div class="empty-state tall">The index has no published segment snapshot yet.</div></section>';
    return;
  }
  const segments = snapshot.segments || [];
  if (!segments.length) {
    target.innerHTML = '<section class="panel"><div class="empty-state tall">No segments have been published.</div></section>';
    return;
  }
  const { total, average, smallest, largest, spread } = segmentStats(segments);
  const cards = segments.map((segment, position) => `
    <article class="segment-card" data-index="${position}">
      <div class="segment-heading">
        <div><strong>Segment ${String(position + 1).padStart(2, '0')}</strong><small>${esc(segment.index_name ? `${segment.index_name} · ${segment.segment_id.split('/').slice(1).join('/')}` : segment.segment_id)}</small></div>
        <span class="badge active">ready</span>
      </div>
      <div class="segment-count"><strong>${fmt(segment.document_count)}</strong><span>documents</span></div>
      <div class="segment-profile">
        <span>Terms <b>${fmt(segment.profile_term_count)}</b></span>
        <span>Entities <b>${fmt(segment.profile_entity_count)}</b></span>
        <span>Topics <b>${fmt(segment.profile_topic_count)}</b></span>
        <span>Local memory <b>${fmt(segment.profile_local_memory_count)}</b></span>
      </div>
    </article>`).join('');

  target.innerHTML = `
    <section class="panel">
      <div class="panel__head">
        <div><p class="eyebrow">Query routing</p><h2>Segment topology</h2></div>
        <span class="panel__note">${esc(snapshot.layout)} · revision ${fmt(index.snapshot_revision)}</span>
      </div>
      <div class="topology-stage">
        ${topologyDiagram(snapshot, segments, largest)}
        <div class="topology-readout">
          <div class="topology-readout__head"><p class="eyebrow">Hovered segment</p><strong id="topology-readout-title">All segments</strong></div>
          <p id="topology-readout-body" class="muted">${fmt(total)} documents across ${fmt(segments.length)} published segments.</p>
          <div class="load-legend">
            <span>Document load</span>
            <div class="load-ramp">${LOAD_RAMP.map(step => `<i style="background:${step}"></i>`).join('')}</div>
            <div class="load-ends"><small>low</small><small>high</small></div>
          </div>
          <div class="distribution-stats">
            <div><span>Smallest</span><strong>${fmt(smallest)}</strong></div>
            <div><span>Average</span><strong>${fmt(Math.round(average))}</strong></div>
            <div><span>Largest</span><strong>${fmt(largest)}</strong></div>
            <div><span>Spread</span><strong>${pct(spread)}</strong></div>
          </div>
        </div>
      </div>
    </section>
    <section class="panel">
      <div class="panel__head">
        <div><p class="eyebrow">Detail</p><h2>All segments</h2></div>
        <span class="panel__note">${fmt(segments.length)} segments · ${fmt(total)} documents</span>
      </div>
      <div class="segment-grid">${cards}</div>
    </section>`;

  wireTopology(target, segments, total);
}

// Hovering a node lifts its spoke and fills the readout; the matching detail
// card highlights too, so the diagram and the list stay tied together.
function wireTopology(target, segments, total) {
  const title = target.querySelector('#topology-readout-title');
  const body = target.querySelector('#topology-readout-body');
  const clear = () => {
    target.querySelectorAll('.tspoke.lit').forEach(spoke => spoke.classList.remove('lit'));
    target.querySelectorAll('.segment-card.lit').forEach(card => card.classList.remove('lit'));
    title.textContent = 'All segments';
    body.textContent = `${fmt(total)} documents across ${fmt(segments.length)} published segments.`;
  };
  target.querySelectorAll('.tnode').forEach(node => {
    const position = Number(node.dataset.index);
    node.addEventListener('mouseenter', () => {
      clear();
      target.querySelector(`#tspoke-${position}`)?.classList.add('lit');
      target.querySelector(`.segment-card[data-index="${position}"]`)?.classList.add('lit');
      const segment = segments[position];
      title.textContent = `Segment ${String(position + 1).padStart(2, '0')}`;
      body.textContent = `${fmt(segment.document_count)} documents · ${fmt(segment.profile_term_count)} terms · ${fmt(segment.profile_entity_count)} entities`;
    });
  });
  target.querySelector('.topology-svg')?.addEventListener('mouseleave', clear);
}

/* ── providers ─────────────────────────────────────────────────── */

function renderProviders(items, projectIndex = projectIndexView(state.status?.index, items)) {
  const visible = items
    .filter(item => item.events_total > 0 || (item.indexes || []).length > 0)
    .sort((left, right) => Number(right.last_seen_ms || 0) - Number(left.last_seen_ms || 0) || left.provider.localeCompare(right.provider));

  const rail = document.querySelector('#rail-providers');
  if (!visible.length) {
    state.provider = null;
    rail.innerHTML = '<p class="rail__empty">No provider telemetry yet.</p>';
    document.querySelector('#provider-view').innerHTML = '<section class="panel"><div class="empty-state tall">Providers appear here once an integration creates an index or sends lifecycle telemetry.</div></section>';
    renderIndexMetrics(items, projectIndex);
    return;
  }
  if (!state.provider || !visible.some(item => item.provider === state.provider)) state.provider = visible[0].provider;

  rail.innerHTML = visible.map(item => {
    const tone = item.state === 'active' ? 'active' : item.state === 'idle' ? 'idle' : '';
    return `<button type="button" class="railprovider ${item.provider === state.provider ? 'selected' : ''}" data-provider="${esc(item.provider)}" title="${esc(item.compiled ? item.state.replace('_', ' ') : 'not compiled')}"><span class="dot ${tone}"></span><span class="railprovider__name">${esc(item.provider)}</span><span class="railprovider__count">${fmt(item.events_total)}</span></button>`;
  }).join('');
  rail.querySelectorAll('.railprovider').forEach(button => button.addEventListener('click', () => {
    state.provider = button.dataset.provider;
    state.tool = null;
    renderProviders(items, projectIndexView(state.status?.index, items));
    showView('providers');
  }));

  renderIndexMetrics(items, projectIndex);
  renderProvider(visible.find(item => item.provider === state.provider));
}

function renderIndexMetrics(providers, projectIndex) {
  const indexes = providers.flatMap(provider => provider.indexes || []);
  const fallback = state.status?.index || {};
  const documents = Number(projectIndex?.snapshot?.global_document_count ?? fallback.source_document_count ?? 0);
  const records = Number(fallback.record_count || 0)
    + indexes.reduce((sum, index) => sum + Number(index.record_count || 0), 0);
  const storeCount = Number(projectIndex?.snapshot?.store_count || (indexes.length ? indexes.length : 1));
  const scope = projectIndex?.snapshot
    ? `${fmt(storeCount)} project ${storeCount === 1 ? 'store' : 'stores'} · ${fmt(projectIndex.snapshot.segment_count)} segments`
    : 'primary query index';
  tween(document.querySelector('#documents'), [documents], ([value]) => fmt(Math.round(value)));
  tween(document.querySelector('#records'), [records], ([value]) => fmt(Math.round(value)));
  document.querySelector('#documents-scope').textContent = scope;
  document.querySelector('#records-scope').textContent = scope;
}

function renderFeed(providers) {
  const events = providers
    .flatMap(provider => (provider.events || []).map(event => ({ ...event, provider: provider.provider })))
    .sort((left, right) => right.timestamp_ms - left.timestamp_ms)
    .slice(0, 12);
  document.querySelector('#feed-note').textContent = events.length ? `${events.length} most recent events` : '';
  document.querySelector('#overview-feed').innerHTML = events.length
    ? events.map(event => `
      <div class="activity-item">
        <i class="activity-dot"></i>
        <div class="event-detail">
          <div class="tool-heading">
            <span class="feed-provider">${esc(event.provider)}</span>
            <strong>${esc(event.tool_name ? toolGroup(event.tool_name) : event.event || 'Lifecycle event')}</strong>
            ${event.tool_name ? `<span>${esc(toolDetail(event))}</span>` : ''}
          </div>
          <small>${esc(event.session_key || 'unknown session')} · ${esc(event.category || 'lifecycle')}</small>
        </div>
        <strong class="topbar__stamp">${clock(event.timestamp_ms)}</strong>
      </div>`).join('')
    : '<div class="empty-state">No provider events captured yet.</div>';
}

function sessionsFor(events) {
  const groups = new Map();
  events.forEach(event => { if (!groups.has(event.session_key)) groups.set(event.session_key, []); groups.get(event.session_key).push(event); });
  return [...groups]
    .map(([session_key, list]) => {
      list.sort((a, b) => a.timestamp_ms - b.timestamp_ms);
      return { session_key, events: list, last_seen_ms: list.at(-1).timestamp_ms, last_event: list.at(-1).event };
    })
    .sort((a, b) => b.last_seen_ms - a.last_seen_ms);
}

// Run-cycle figures. With `counters` (the server's cumulative per-provider
// counters) every figure covers the whole ledger. Without them (older server
// builds) the figures come from the retained event window only, and the
// result carries `scope` text naming that window so the page never presents
// a window count as a total. A figure that cannot be known is `null`.
function cycleAnalytics(events, counters = null) {
  const times = events.map(event => Number(event.timestamp_ms)).filter(value => value > 0);
  const from = times.length ? Math.min(...times) : null;
  const to = times.length ? Math.max(...times) : null;
  const span = ms => {
    const s = Math.max(0, ms) / 1000;
    if (s < 90) return `${Math.round(s)}s`;
    if (s < 5400) return `${Math.round(s / 60)} min`;
    if (s < 172800) return `${(s / 3600).toFixed(1)} h`;
    return `${Math.round(s / 86400)} days`;
  };
  const retained = {
    count: events.length, from, to,
    label: events.length ? `last ${fmt(events.length)} events, ${span(to - from)}` : 'no retained events'
  };
  if (counters) {
    const prompts = Number(counters.prompts || 0);
    const completed = Number(counters.cycles_completed || 0);
    const toolCalls = Number(counters.tool_calls || 0);
    const since = Number(counters.since_ms || 0);
    return {
      cumulative: true,
      window: retained,
      scope: since ? `Since ${new Date(since).toLocaleString()}` : 'Cumulative',
      prompts,
      toolCalls,
      responses: Number(counters.tool_results || 0),
      stops: Number(counters.stops || 0),
      completed,
      completionRate: prompts ? Math.min(1, completed / prompts) : null,
      averageTools: prompts ? toolCalls / prompts : null,
      averageDuration: counters.cycle_duration_avg_ms ?? null,
      p50Duration: counters.cycle_duration_p50_ms ?? null,
      p95Duration: counters.cycle_duration_p95_ms ?? null,
      reason: prompts ? '' : 'No prompts recorded yet.',
      tokenUsage: tokenAccounting([], prompts, counters)
    };
  }
  const isPrompt = event => /^(UserPromptSubmit|BeforeAgent)$/i.test(event.event);
  const isStop = event => /^(Stop|AfterAgent|SessionEnd)$/i.test(event.event);
  const prompts = events.filter(isPrompt);
  const toolCalls = events.filter(event => /^(PreToolUse|BeforeTool)$/i.test(event.event));
  const responses = events.filter(event => event.tool_response_preview);
  const stops = events.filter(event => /^(Stop|AfterAgent)$/i.test(event.event));
  const usageEvents = events.filter(event => [event.input_tokens, event.output_tokens, event.total_tokens].some(Number.isFinite));
  const durations = [];
  sessionsFor(events).forEach(session => session.events.forEach((event, index) => {
    if (!isPrompt(event)) return;
    const stop = session.events.slice(index + 1).find(isStop);
    if (stop) durations.push(Math.max(0, stop.timestamp_ms - event.timestamp_ms));
  }));
  return {
    cumulative: false,
    window: retained,
    scope: `Window: ${retained.label}`,
    prompts: prompts.length,
    toolCalls: toolCalls.length,
    responses: responses.length,
    stops: stops.length,
    completed: durations.length,
    completionRate: prompts.length ? durations.length / prompts.length : null,
    averageTools: prompts.length ? toolCalls.length / prompts.length : null,
    averageDuration: durations.length ? durations.reduce((sum, value) => sum + value, 0) / durations.length : null,
    p50Duration: null,
    p95Duration: null,
    reason: prompts.length ? '' : `No prompts in the ${retained.label}. Busy providers fill the window with retrieval and tool events; totals need a server build with cumulative counters.`,
    tokenUsage: tokenAccounting(usageEvents, prompts.length)
  };
}

// The three input counters are DISJOINT: `input_tokens` is the uncached
// remainder only, with cached context reported separately as cache writes and
// cache reads. Real context sent = all three summed. The provider's own
// `total_tokens` ignores both cache fields, so it is recomputed here rather
// than trusted — reading it directly under-reports a cached turn enormously.
// With `counters`, the sums come from the server's cumulative token counters,
// which use the same disjoint fields.
function tokenAccounting(usageEvents, promptCount, counters = null) {
  const sum = field => counters
    ? Number(counters[field] || 0)
    : usageEvents.reduce((total, event) => total + Number(event[field] || 0), 0);
  const fresh = sum('input_tokens');
  const cacheWrite = sum('cache_creation_input_tokens');
  const cacheRead = sum('cache_read_input_tokens');
  const output = sum('output_tokens');
  const contextIn = fresh + cacheWrite + cacheRead;
  const total = contextIn + output;
  return {
    available: counters ? Number(counters.usage_events || 0) > 0 : usageEvents.length > 0,
    fresh, cacheWrite, cacheRead, output, contextIn, total,
    cacheHitRate: contextIn ? cacheRead / contextIn : 0,
    averageTotal: promptCount ? total / promptCount : null,
    reportedTotal: counters ? Number(counters.reported_total_tokens || 0) : sum('total_tokens')
  };
}

function delegationAnalytics(events) {
  const starts = events.filter(event => event.event === 'SubagentStart');
  const stops = events.filter(event => event.event === 'SubagentStop');
  const runs = inferSubagentEnds(pairSubagentRuns(starts, stops), events);
  const usageEvents = events.filter(event => event.event === 'TurnUsage');
  attributeSubagentUsage(runs, usageEvents).forEach((usage, key) => {
    runs.push({ start: { agent_id: key }, stop: null, duration: null, usageOnly: true, usage });
  });
  const completed = runs.filter(run => run.stop);
  return {
    starts: starts.length,
    completed: completed.length,
    active: runs.filter(run => !run.stop && !run.interrupted && !run.ended && !run.usageOnly).length,
    unfinished: runs.filter(run => run.interrupted || run.ended === 'session_ended').length,
    averageDuration: completed.length ? completed.reduce((sum, run) => sum + run.duration, 0) / completed.length : 0,
    hasUsage: runs.some(run => run.usage),
    runs: runs.sort((a, b) => (b.start.timestamp_ms || 0) - (a.start.timestamp_ms || 0)).slice(0, 8)
  };
}

function renderCyclePanel(stats) {
  const step = (label, value, note) => `<div class="flow-step"><em>${value}</em><strong>${label}</strong><small>${note}</small></div>`;
  const known = (value, format) => value == null ? '—' : format(value);
  const cycleNote = stats.p50Duration != null
    ? `<small>p50 ≤ ${seconds(stats.p50Duration)} · p95 ≤ ${seconds(stats.p95Duration)}</small>` : '';
  return `
    <section class="panel">
      <div class="panel__head">
        <div><p class="eyebrow">Agent run cycle</p><h2>Execution flow</h2></div>
        <span class="panel__note">Prompt to final response</span>
      </div>
      <p class="metric-scope ${stats.cumulative ? '' : 'metric-scope--window'}">${esc(stats.scope)}</p>
      <div class="flow">
        ${step('Prompts', fmt(stats.prompts), 'submitted')}
        ${step('Tool work', fmt(stats.toolCalls), 'invocations')}
        ${step('Responses', fmt(stats.responses), 'returned')}
        ${step('Stops', fmt(stats.stops), 'completed')}
      </div>
      <div class="figure-row">
        <div><span>Completion rate</span><strong>${known(stats.completionRate, pct)}</strong></div>
        <div><span>Avg. tools / prompt</span><strong>${known(stats.averageTools, value => value.toFixed(1))}</strong></div>
        <div><span>Avg. cycle time</span><strong>${known(stats.averageDuration, seconds)}</strong>${cycleNote}</div>
      </div>
      ${stats.reason ? `<p class="metric-reason">${esc(stats.reason)}</p>` : ''}
    </section>`;
}

function renderTokenPanel(stats) {
  const usage = stats.tokenUsage;
  const head = `
    <div class="panel__head">
      <div><p class="eyebrow">Token usage</p><h2>Context accounting</h2></div>
      <span class="panel__note">Cache-aware</span>
    </div>
    <p class="metric-scope ${stats.cumulative ? '' : 'metric-scope--window'}">${esc(stats.scope)}</p>`;
  if (!usage.available) {
    const reason = stats.cumulative
      ? 'No token usage has been reported since counting began.'
      : `— No token usage in the ${stats.window.label}. Totals beyond this window need a server build with cumulative counters.`;
    return `<section class="panel">${head}<div class="empty-state">${esc(reason)}</div></section>`;
  }
  const value = (label, amount, note) => `<div><span>${label}</span><strong>${amount}</strong><small>${note}</small></div>`;
  const parts = [
    { key: 'fresh', label: 'Fresh input', amount: usage.fresh },
    { key: 'write', label: 'Cache write', amount: usage.cacheWrite },
    { key: 'read', label: 'Cache read', amount: usage.cacheRead }
  ].filter(part => part.amount > 0);
  // Part-to-whole across the context window: a stacked bar, with a 2px surface
  // gap between fills and every segment directly labelled in the legend.
  const composition = usage.contextIn ? `
    <div class="token-bar" role="img" aria-label="Context composition: ${parts.map(part => `${part.label} ${fmt(part.amount)} tokens`).join(', ')}">
      ${parts.map(part => `<i class="token-bar__part token-bar__part--${part.key}" style="flex:${part.amount}"></i>`).join('')}
    </div>
    <div class="token-key">
      ${parts.map(part => `<span><i class="token-bar__part--${part.key}"></i>${part.label} <b>${pct(part.amount / usage.contextIn)}</b></span>`).join('')}
    </div>` : '';
  return `
    <section class="panel">${head}
      <div class="figure-row" style="margin-top:0;padding-top:0;border-top:0">
        ${value('Context in', fmt(usage.contextIn), 'fresh + cached')}
        ${value('Output', fmt(usage.output), 'response generated')}
        ${value('Total', fmt(usage.total), 'context + output')}
        ${value('Cache hit rate', pct(usage.cacheHitRate), 'context served from cache')}
      </div>
      ${composition}
      <p class="token-note">${usage.averageTotal == null ? 'Avg. per prompt unknown: no prompts counted.' : `Avg. ${fmt(Math.round(usage.averageTotal))} tokens / prompt.`} Provider-reported total is ${fmt(usage.reportedTotal)} — it counts neither cache reads nor cache writes.</p>
    </section>`;
}

// `events` should include the milestone events so subagent runs are not
// evicted by tool bursts. With `counters`, the headline counts are cumulative
// and the rows show the most recent runs. Active never comes from starts minus
// stops, which would count an interrupted run as active forever. Servers that
// track open subagents give cumulative Active and Did not finish figures;
// otherwise both come from the paired runs in the retained events.
function renderDelegationPanel(events, counters = null) {
  const stats = delegationAnalytics(events);
  if (counters) {
    stats.starts = Number(counters.subagent_starts || 0);
    stats.completed = Number(counters.subagent_stops || 0);
  }
  const tracked = Boolean(counters?.open_subagents);
  let quiet = 0;
  if (tracked) {
    const now = Date.now();
    const open = Object.values(counters.open_subagents);
    stats.active = open.filter(entry => now - Number(entry.last_seen_ms || 0) <= SUBAGENT_IDLE_MS).length;
    quiet = open.length - stats.active;
    stats.unfinished = Number(counters.subagent_interrupted || 0) + Number(counters.subagent_ended_without_stop || 0);
  }
  if (!stats.starts && !stats.completed && !stats.hasUsage) return '';
  const rowState = run => run.stop ? ['completed', 'completed', seconds(run.duration)]
    : run.interrupted ? ['interrupted', 'interrupted', 'did not finish']
    : run.ended === 'session_ended' ? ['ended', 'ended', 'ended without stop']
    : run.ended === 'idle' ? ['quiet', 'quiet', 'no recent activity']
    : run.usageOnly ? ['usage', 'usage only', 'no lifecycle events']
    : ['running', 'running', 'in progress'];
  const rows = stats.runs.map(run => {
    const label = run.start.agent_type || run.start.agent_id || 'Subagent';
    const id = run.start.agent_type ? run.start.agent_id || '' : '';
    const [tone, status, note] = rowState(run);
    const usage = run.usage;
    const usageTotal = usage ? usage.fresh + usage.cacheWrite + usage.cacheRead + usage.output : 0;
    return `
      <div class="delegation-row">
        <div><strong>${esc(label)}</strong><small>${esc(id || 'delegated task')} · ${clock(run.start.timestamp_ms)}${usage ? ` · ${fmt(usageTotal)} tokens` : ''}</small></div>
        <span class="delegation-duration ${tone}">${note}</span>
        <span class="badge ${run.stop ? 'active' : 'idle'}">${status}</span>
      </div>`;
  }).join('');
  const scope = !counters ? 'Counts from the retained event window only.'
    : tracked ? `Counts since counting began; rows are the most recent runs. Did not finish counts runs that were resumed or whose session ended without a stop.${quiet ? ` ${fmt(quiet)} open ${quiet === 1 ? 'run has' : 'runs have'} had no activity for 30 minutes and ${quiet === 1 ? 'is' : 'are'} in neither Active nor Did not finish.` : ''}`
    : 'Started and Completed count since counting began; Active and Did not finish come from recent events, so they need not add up.';
  return `
    <section class="panel">
      <div class="panel__head">
        <div><p class="eyebrow">Delegation</p><h2>Subagent activity</h2></div>
        <span class="panel__note">Lifecycle hooks</span>
      </div>
      <div class="figure-row" style="margin-top:0;padding-top:0;border-top:0">
        <div><span>Started</span><strong>${fmt(stats.starts)}</strong></div>
        <div><span>Completed</span><strong>${fmt(stats.completed)}</strong></div>
        <div><span>Active</span><strong>${fmt(stats.active)}</strong></div>
        <div><span>Did not finish</span><strong>${fmt(stats.unfinished)}</strong></div>
        <div><span>Avg. duration</span><strong>${seconds(stats.averageDuration)}</strong><small>recent runs</small></div>
      </div>
      <p class="metric-scope ${tracked ? '' : 'metric-scope--window'}">${scope}</p>
      <div>${rows}</div>
    </section>`;
}

function renderStores(indexes) {
  const groups = ['mcp', 'memory'].map(role => {
    const roleIndexes = indexes.filter(index => index.role === role);
    if (!roleIndexes.length) return '';
    const cards = roleIndexes.map(index => `
      <article class="store-card">
        <div class="segment-heading">
          <div><strong>${role === 'mcp' ? 'Legacy query index' : 'Captured memories'}</strong><small>${esc(index.name)}</small></div>
          <span class="badge ${index.dirty ? 'idle' : 'active'}">${index.dirty ? 'dirty' : 'ready'}</span>
        </div>
        <div class="store-counts">
          <span><b>${fmt(index.source_document_count)}</b> documents</span>
          <span><b>${fmt(index.record_count)}</b> records</span>
          <span><b>${fmt(index.snapshot?.segment_count || 0)}</b> segments</span>
        </div>
        <small>Revision ${fmt(index.snapshot_revision)}</small>
      </article>`).join('');
    return `
      <div class="store-group">
        <div class="store-group-heading">
          <strong>${role === 'mcp' ? 'Legacy MCP store' : 'Captured memory store'}</strong>
          <span>${roleIndexes.length} ${roleIndexes.length === 1 ? 'index' : 'indexes'}</span>
        </div>
        <div class="store-grid">${cards}</div>
      </div>`;
  }).join('');
  return groups || '<div class="empty-state">No provider memory directories with metadata.json were found.</div>';
}

function renderProvider(provider) {
  if (!provider) return;
  const events = (provider.events || []).slice().sort((a, b) => b.timestamp_ms - a.timestamp_ms);
  const toolEvents = events.filter(event => event.tool_name);
  const sessions = sessionsFor(events);
  const indexes = provider.indexes || [];
  const current = sessions[0];
  const live = current && Date.now() - current.last_seen_ms < 60_000;

  const counts = toolEvents.reduce((result, event) => {
    const group = toolGroup(event.tool_name);
    result[group] = (result[group] || 0) + 1;
    return result;
  }, {});
  const tools = Object.entries(counts).sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  if (state.tool && !counts[state.tool]) state.tool = null;

  const detailsByGroup = toolEvents.reduce((result, event) => {
    const group = toolGroup(event.tool_name);
    result[group] ||= {};
    const detail = toolDetail(event);
    result[group][detail] = (result[group][detail] || 0) + 1;
    return result;
  }, {});
  const calls = toolEvents.filter(event => !state.tool || toolGroup(event.tool_name) === state.tool).slice(0, 30);
  const details = Object.entries(detailsByGroup[state.tool] || {}).sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  // Cumulative counters exist only on newer servers; older ones send none and
  // every figure falls back to the retained window, labelled as such.
  const counters = provider.counters || null;
  const stats = cycleAnalytics(events, counters);
  const windowLabel = stats.window.label;
  // Milestone events (prompts, stops, usage, subagents) are kept apart from
  // the window on newer servers so delegation rows survive tool bursts.
  const seen = new Set(events.map(event => event.event_id));
  const withMilestones = events.concat((provider.milestone_events || []).filter(event => !seen.has(event.event_id)));
  const toolCallTotal = counters ? Number(counters.tool_calls || 0) : toolEvents.filter(event => /^(PreToolUse|BeforeTool)$/i.test(event.event)).length;

  document.querySelector('#provider-view').innerHTML = `
    <div class="provider-head">
      <div>
        <div class="provider-head__id">
          <h2>${esc(provider.provider)}</h2>
          <span class="badge ${esc(provider.state)}">${provider.compiled ? esc(provider.state.replace('_', ' ')) : 'not compiled'}</span>
        </div>
        <p>${esc(provider.note)}</p>
      </div>
      <div class="provider-head__last"><span>Last activity</span><strong>${clock(provider.last_seen_ms)}</strong></div>
    </div>

    <div class="row row--halves">${renderCyclePanel(stats)}${renderTokenPanel(stats)}</div>
    ${renderDelegationPanel(withMilestones, counters)}

    <div class="stat-row">
      <article class="stat"><span>Documents</span><strong>${fmt(indexes.reduce((sum, index) => sum + Number(index.source_document_count || 0), 0))}</strong><small>${fmt(indexes.length)} private memory stores</small></article>
      <article class="stat"><span>Records</span><strong>${fmt(indexes.reduce((sum, index) => sum + Number(index.record_count || 0), 0))}</strong><small>across private memory stores</small></article>
      <article class="stat"><span>Sessions</span><strong>${fmt(provider.sessions_started)}</strong><small>started in total · ${fmt(provider.sessions_active)} active · ${fmt(sessions.length)} in ${esc(windowLabel)}</small></article>
      <article class="stat"><span>Tool calls</span><strong>${fmt(toolCallTotal)}</strong><small>${counters ? 'in total' : `in ${esc(windowLabel)}`} · ${fmt(tools.length)} categories recently</small></article>
    </div>

    <div class="row row--narrow-first">
      <section class="panel">
        <div class="sub-head">
          <div><p class="eyebrow">Session history</p><h2>Sessions</h2></div>
          <span class="panel__note">${sessions.length ? `${sessions.length} in ${esc(windowLabel)}` : 'None in the retained window'}</span>
        </div>
        <div class="scroll-list">${renderSessions(sessions)}</div>
      </section>
      <section class="panel">
        <div class="sub-head">
          <div><p class="eyebrow">Live session</p><h2>${live ? 'Current activity' : 'Latest activity'}</h2></div>
          <span class="live-indicator ${live ? 'on' : ''}"><i></i>${live ? 'LIVE' : 'IDLE'}</span>
        </div>
        ${renderLive(current)}
      </section>
    </div>

    <div class="row row--halves">
      <section class="panel">
        <div class="sub-head">
          <div><p class="eyebrow">Private memory</p><h2>Memory stores</h2></div>
          <span class="panel__note">${indexes.length ? `${indexes.length} discovered` : 'None found'}</span>
        </div>
        ${renderStores(indexes)}
      </section>
      <section class="panel">
        <div class="sub-head">
          <div><p class="eyebrow">Tool categories</p><h2>Frequently used tools</h2></div>
          <span class="panel__note">${esc(windowLabel)} · select for details</span>
        </div>
        <div class="tool-grid">${tools.length ? tools.map(([name, count]) => {
          const top = Object.entries(detailsByGroup[name] || {}).sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])).slice(0, 3);
          return `<button type="button" class="tool-card ${state.tool === name ? 'selected' : ''}" data-tool="${esc(name)}">
            <div class="tool-card-title"><strong>${esc(name)}</strong><span>${fmt(count)}</span></div>
            <div class="tool-meter"><i style="width:${Math.max(6, count / tools[0][1] * 100)}%"></i></div>
            ${top.length ? `<div class="tool-breakdown">${top.map(([detail, detailCount]) => `<div><span>${esc(detail)}</span><strong>${fmt(detailCount)}</strong></div>`).join('')}</div>` : ''}
          </button>`;
        }).join('') : '<div class="empty-state">No tool calls captured yet.</div>'}</div>
        ${state.tool ? `<div class="detail-breakdown"><div class="breakdown-heading"><span>${esc(state.tool)} details</span><span>${details.length} distinct</span></div>${details.map(([name, count]) => `<div class="breakdown-row"><span>${esc(name)}</span><strong>${fmt(count)}</strong></div>`).join('')}</div>` : ''}
      </section>
    </div>

    <section class="panel">
      <div class="sub-head">
        <div><p class="eyebrow">Call history</p><h2>${state.tool ? esc(state.tool) : 'All tool categories'}</h2></div>
        <span class="panel__note">${calls.length} most recent · ${esc(windowLabel)}</span>
      </div>
      <div>${calls.length ? calls.map(event => renderToolCall(event, 'history')).join('') : '<div class="empty-state">No tool calls match this filter.</div>'}</div>
    </section>`;

  document.querySelectorAll('.tool-card').forEach(button => button.addEventListener('click', () => {
    state.tool = state.tool === button.dataset.tool ? null : button.dataset.tool;
    renderProvider(provider);
  }));
  document.querySelectorAll('[data-flip-card]').forEach(card => {
    const flip = () => card.classList.toggle('flipped');
    card.addEventListener('click', flip);
    card.addEventListener('keydown', event => {
      if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); flip(); }
    });
  });
}

function renderSessions(sessions) {
  if (!sessions.length) return '<div class="empty-state">No provider sessions observed yet.</div>';
  return sessions.slice(0, 12).map((session, index) => `
    <div class="session-item ${index === 0 ? 'latest' : ''}">
      <div><span class="session-key">${esc(session.session_key)}</span><small>${fmt(session.events.length)} events · ${esc(session.last_event)}</small></div>
      <strong>${clock(session.last_seen_ms)}</strong>
    </div>`).join('');
}

function renderLive(session) {
  if (!session) return '<div class="empty-state tall">Start a provider session to see live activity here.</div>';
  const events = session.events.slice().reverse().slice(0, 8);
  return `
    <div class="live-summary">
      <div><span class="session-key">${esc(session.session_key)}</span><small>${fmt(session.events.length)} events in this session</small></div>
      <strong>${clock(session.last_seen_ms)}</strong>
    </div>
    <div class="live-list">${events.map(event => renderEventDetail(event, 'live')).join('')}</div>`;
}

/* ── command palette ───────────────────────────────────────────── */

const palette = document.querySelector('#palette');
const paletteInput = document.querySelector('#palette-input');
const paletteResults = document.querySelector('#palette-results');
let paletteItems = [];
let paletteActive = 0;

function paletteCommands() {
  const commands = Object.entries(VIEWS).map(([view, meta]) => ({
    label: meta.title, hint: 'View', run: () => showView(view)
  }));
  (state.status?.integrations || [])
    .filter(item => item.events_total > 0 || (item.indexes || []).length > 0)
    .forEach(item => commands.push({
      label: item.provider,
      hint: `Provider · ${fmt(item.events_total)} events`,
      run: () => { state.provider = item.provider; state.tool = null; renderProviders(state.status.integrations || []); showView('providers'); }
    }));
  commands.push({ label: 'Refresh now', hint: 'Action', run: refresh });
  return commands;
}

function renderPalette() {
  const query = paletteInput.value.trim().toLowerCase();
  paletteItems = paletteCommands().filter(item => !query || item.label.toLowerCase().includes(query));
  paletteActive = Math.min(paletteActive, Math.max(0, paletteItems.length - 1));
  paletteResults.innerHTML = paletteItems.length
    ? paletteItems.map((item, index) => `<button type="button" role="option" aria-selected="${index === paletteActive}" class="palette__item ${index === paletteActive ? 'active' : ''}" data-index="${index}"><span>${esc(item.label)}</span><small>${esc(item.hint)}</small></button>`).join('')
    : '<div class="empty-state">Nothing matches.</div>';
  paletteResults.querySelectorAll('.palette__item').forEach(button => {
    button.addEventListener('mousemove', () => { paletteActive = Number(button.dataset.index); renderPalette(); });
    button.addEventListener('click', () => runPalette(Number(button.dataset.index)));
  });
}

function openPalette() {
  palette.hidden = false;
  paletteInput.value = '';
  paletteActive = 0;
  renderPalette();
  paletteInput.focus();
}

function closePalette() { palette.hidden = true; }

function runPalette(index) {
  const item = paletteItems[index];
  if (!item) return;
  closePalette();
  item.run();
}

paletteInput.addEventListener('input', () => { paletteActive = 0; renderPalette(); });
palette.addEventListener('click', event => { if (event.target === palette) closePalette(); });
window.addEventListener('keydown', event => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
    event.preventDefault();
    palette.hidden ? openPalette() : closePalette();
    return;
  }
  if (palette.hidden) return;
  if (event.key === 'Escape') { event.preventDefault(); closePalette(); }
  if (event.key === 'ArrowDown') { event.preventDefault(); paletteActive = (paletteActive + 1) % (paletteItems.length || 1); renderPalette(); }
  if (event.key === 'ArrowUp') { event.preventDefault(); paletteActive = (paletteActive - 1 + paletteItems.length) % (paletteItems.length || 1); renderPalette(); }
  if (event.key === 'Enter') { event.preventDefault(); runPalette(paletteActive); }
});

/* ── boot ──────────────────────────────────────────────────────── */

showView(location.hash.slice(1) || 'overview');
refresh();
startLiveStream();
setInterval(refresh, 5000);
window.addEventListener('resize', drawSparks);
