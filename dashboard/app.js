import { renderEventDetail, renderToolCall, toolDetail, toolGroup } from './event-detail.js';
import { renderCaptureProgressHtml } from './capture-progress.mjs';
import { renderSearchEvaluationHtml } from './search-evaluation.mjs';
import { escapeHtml as esc, mergeBoardPosts, parseBoardLinkTarget } from './board-ui.mjs';

const VIEWS = {
  overview: { title: 'Overview', sub: 'How the memory index is serving agents right now.' },
  index: { title: 'Index', sub: 'Segment topology and document distribution of the primary query index.' },
  providers: { title: 'Providers', sub: 'Per-integration sessions, tool use, and private memory stores.' },
  boards: { title: 'Boards', sub: 'Project structure and updates posted by agents.' },
  flows: { title: 'Flows', sub: 'Provider-neutral workflow runs, task state, and review history.' },
  reviews: { title: 'Search reviews', sub: 'Submitted feedback and offline ranking evaluation.' }
};

const state = { view: 'overview', status: null, series: [], provider: null, tool: null, hover: null, reviewOffset: 0, reviewTotal: 0, boards: [], selectedBoardId: null, boardSnapshot: null, boardTrail: [], boardGeneration: 0, boardRenderSeq: 0, boardTick: 0, linkedBoards: new Map(), flows: [], selectedFlowId: null, flowSnapshot: null };

const tokenInput = document.querySelector('#token');
tokenInput.value = localStorage.getItem('membrick-dashboard-token') || '';
tokenInput.addEventListener('change', () => localStorage.setItem('membrick-dashboard-token', tokenInput.value));
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
  if (loadData && view === 'flows') loadFlows();
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

function sortBoards(boards) {
  return (boards || []).slice().sort((a, b) => String(a.title || a.key).localeCompare(String(b.title || b.key)));
}

// The list is redrawn only when its visible content changes, so polling does
// not disturb the selection or the sidebar scroll.
function renderBoardList() {
  const list = document.querySelector('#boards-list');
  const signature = JSON.stringify(state.boards.map(board => [board.board_id, board.title, board.key, board.type]));
  document.querySelector('#boards-count').textContent = `${fmt(state.boards.length)} boards`;
  if (list.dataset.signature !== signature) {
    list.dataset.signature = signature;
    list.innerHTML = state.boards.length ? state.boards.map(board => `
      <button class="board-choice" type="button" data-board-id="${esc(board.board_id)}">
        <span class="board-choice__top"><strong>${esc(board.title || board.key)}</strong>${board.type && board.type !== 'general' ? `<span class="board-choice__type">${esc(board.type)}</span>` : ''}${board.board_template ? '<span class="board-choice__type board-choice__type--template">template</span>' : ''}</span>
        <small class="board-choice__key">${esc(board.key)}</small>
        <span class="board-choice__meta" data-board-meta="${esc(board.board_id)}">${esc(boardMetaText(board.board_id))}</span>
      </button>`).join('') : '<div class="empty-state">No boards yet. Agents can open a project board and link planning, task, and milestone boards.</div>';
    list.querySelectorAll('[data-board-id]').forEach(button => button.addEventListener('click', () => {
      state.boardTrail = [];
      selectBoard(button.dataset.boardId);
    }));
  }
  list.querySelectorAll('.board-choice').forEach(button => button.classList.toggle('is-selected', button.dataset.boardId === state.selectedBoardId));
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
  document.querySelectorAll('.board-choice').forEach(button => button.classList.toggle('is-selected', button.dataset.boardId === boardId));
  document.querySelector('#board-detail').innerHTML = '<div class="empty-state">Loading board…</div>';
  state.boardSnapshot = null;
  await loadBoardPosts(boardId, null, false);
  let snapshot = state.boardSnapshot;
  if (snapshot?.latestSequence > lastPostSequence(snapshot.posts)) {
    if (await loadLatestBoardPosts(boardId)) await renderBoardDetail(snapshot);
  }
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
    if (!focusBoardPost(postSequence)) {
      const notice = document.createElement('p');
      notice.className = 'board-link-error';
      notice.textContent = `Post ${postSequence} was not found on this board.`;
      document.querySelector('#board-detail')?.prepend(notice);
    }
  }
}

function focusBoardPost(sequence) {
  const post = document.querySelector(`#post-${sequence}`);
  if (!post) return false;
  post.closest('details')?.setAttribute('open', '');
  post.scrollIntoView({ behavior: stillness.matches ? 'auto' : 'smooth', block: 'center' });
  post.classList.add('is-focused');
  setTimeout(() => post.classList.remove('is-focused'), 2400);
  return true;
}

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

/* Change stream: the server sends one notice per board or Flow write. Each
   notice names what changed; the page refetches only that through the read
   APIs. fetch() is used instead of EventSource so the bearer token is sent. */

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
    const response = await fetch('/api/boards/events', {
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
  const signature = JSON.stringify(state.flows.map(({ run = {} }) => [run.flow_id, run.state, run.stage, run.goal, run.template_version, flowTemplateName(run)]));
  document.querySelector('#flows-count').textContent = `${fmt(state.flows.length)} flows`;
  if (list.dataset.signature !== signature) {
    list.dataset.signature = signature;
    list.innerHTML = state.flows.length ? state.flows.map(snapshot => {
      const run = snapshot.run || {};
      return `<button class="flow-choice" type="button" data-flow-id="${esc(run.flow_id || '')}">
        <span class="flow-choice__meta"><span class="flow-state flow-state--${esc(run.state || 'unknown')}">${esc(run.state || 'unknown')}</span><small>${esc(run.stage || 'No stage')}</small></span>
        <strong>${esc(run.goal || 'Untitled Flow')}</strong>
        <small>${esc(flowTemplateName(run))} · v${esc(run.template_version || '—')}</small>
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
    const flowId = state.selectedFlowId;
    if (!flowId || !state.flows.some(snapshot => snapshot.run?.flow_id === flowId)) return;
    const detail = await api(`/api/flows/${encodeURIComponent(flowId)}`);
    if (state.selectedFlowId !== flowId || state.view !== 'flows') return;
    const next = { run: detail.flow, events: detail.events || [] };
    if (JSON.stringify(next) === JSON.stringify(state.flowSnapshot)) return;
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
  document.querySelectorAll('.flow-choice').forEach(button => button.classList.toggle('is-selected', button.dataset.flowId === flowId));
  document.querySelector('#flow-detail').innerHTML = '<div class="empty-state">Loading Flow history…</div>';
  try {
    const result = await api(`/api/flows/${encodeURIComponent(flowId)}`);
    if (state.selectedFlowId !== flowId) return;
    state.flowSnapshot = { run: result.flow, events: result.events || [] };
    renderFlowDetail(state.flowSnapshot);
  } catch (error) {
    if (state.selectedFlowId === flowId) document.querySelector('#flow-detail').innerHTML = `<div class="empty-state">Could not load Flow: ${esc(error.message)}</div>`;
  }
}

function flowTaskEntries(tasks) {
  if (Array.isArray(tasks)) return tasks;
  if (tasks && typeof tasks === 'object') return Object.entries(tasks).map(([taskId, task]) => ({ task_id: taskId, ...task }));
  return [];
}

function flowDataRows(data) {
  if (data == null || typeof data !== 'object' || Array.isArray(data)) return `<p>${esc(data == null ? '' : JSON.stringify(data))}</p>`;
  const entries = Object.entries(data).filter(([key]) => !['coordinator_fence'].includes(key));
  if (!entries.length) return '<span class="muted">No additional event data</span>';
  return `<dl>${entries.map(([key, value]) => `<div><dt>${esc(key.replaceAll('_', ' '))}</dt><dd>${esc(Array.isArray(value) ? value.join(', ') : typeof value === 'string' ? value : JSON.stringify(value))}</dd></div>`).join('')}</dl>`;
}

function flowBoardLabel(boardId) {
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

function renderFlowDetail(snapshot) {
  const run = snapshot.run || {};
  const events = snapshot.events || [];
  const tasks = flowTaskEntries(run.tasks);
  const references = [{ kind: 'board', id: run.board_id, relation: 'flow board' }, ...(run.references || [])]
    .filter(ref => ref && ref.id)
    .filter((ref, index, all) => all.findIndex(other => other.id === ref.id) === index);
  const referenceHtml = references.length ? `<div class="flow-references">${references.map(ref => {
    const kind = String(ref.kind || '').toLowerCase();
    const postReference = kind === 'post' || String(ref.id).includes(':post:');
    const boardId = postReference ? String(ref.id).split(':post:')[0] : ref.id;
    const postSequence = postReference ? Number(String(ref.id).split(':post:').at(-1)) : null;
    const label = `${flowBoardLabel(boardId)}${Number.isFinite(postSequence) ? ` · post ${postSequence}` : ''}`;
    return `<span class="flow-reference"><small>${esc(ref.relation || ref.kind || 'reference')}</small>${postReference || kind === 'board'
      ? `<button type="button" class="board-link" data-flow-board-id="${esc(boardId)}"${Number.isFinite(postSequence) ? ` data-flow-post-sequence="${postSequence}"` : ''}>${esc(label)}</button>`
      : `<span class="mono">${esc(label)}</span>`}</span>`;
  }).join('')}</div>` : '<span class="muted">No board references</span>';
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
  }).join('') : '<div class="empty-state">No task events recorded for this Flow yet.</div>';
  const timelineHtml = events.length ? events.slice().reverse().map(event => `
    <article class="flow-event">
      <div class="flow-event__head"><div><span class="flow-event__kind">${esc(String(event.kind || 'event').replaceAll('_', ' '))}</span>${event.task_id ? `<span class="flow-event__task">Task ${esc(event.task_id)}</span>` : ''}</div><time>${esc(event.timestamp ? new Date(event.timestamp).toLocaleString() : 'Time unavailable')}</time></div>
      <p class="flow-event__actor">Recorded by ${esc(event.actor || 'unknown actor')} · ${esc(event.provider || 'unknown provider')}${event.attempt_id ? ` · attempt ${esc(event.attempt_id)}` : ''} · revision ${fmt(event.revision || 0)}</p>
      ${event.post_id ? `<div class="flow-event__post"><small>Associated board post</small>${flowPostLink(event.post_id, 'Open associated post')}</div>` : ''}
      <div class="flow-event__data">${flowDataRows(event.data)}</div>
    </article>`).join('') : '<div class="empty-state">No Flow events recorded yet.</div>';
  document.querySelector('#flow-detail').innerHTML = `
    <div class="flow-detail__head"><div><p class="eyebrow">Flow · ${esc(run.flow_id || '')}</p><h2>${esc(run.goal || 'Untitled Flow')}</h2><p class="muted">Created by ${esc(run.created_by || 'unknown agent')} · ${esc(run.created_at ? new Date(run.created_at).toLocaleString() : 'date unavailable')}</p></div><span class="flow-source-label">Flow projection · Membrick recorded</span></div>
    <div class="flow-summary">
      <div><small>Template</small><strong>${esc(flowTemplateName(run))} · v${esc(run.template_version || '—')}</strong>${run.flow_template?.content_hash ? `<small class="mono">${esc(run.flow_template.content_hash)}</small>` : ''}</div>
      <div><small>Current stage</small><strong>${esc(run.stage || '—')}</strong></div>
      <div><small>Run state</small><strong><span class="flow-state flow-state--${esc(run.state || 'unknown')}">${esc(run.state || 'unknown')}</span></strong></div>
      <div><small>Revision</small><strong>${fmt(run.revision || 0)}</strong></div>
      <div><small>Coordinator</small><strong>${esc(run.coordinator?.agent_id || 'Unclaimed')}</strong><small>${esc(run.coordinator?.provider || '')} · fence ${fmt(run.coordinator_fence || 0)}</small></div>
      <div><small>Updated</small><strong>${esc(run.updated_at ? new Date(run.updated_at).toLocaleString() : '—')}</strong></div>
    </div>
    <section class="flow-detail__section"><div class="panel__head"><div><p class="eyebrow">Context</p><h3>Linked boards and posts</h3></div></div>${referenceHtml}</section>
    <section class="flow-detail__section"><div class="panel__head"><div><p class="eyebrow">Current projection</p><h3>Tasks · ${fmt(tasks.length)}</h3></div></div><div class="flow-tasks">${taskHtml}</div></section>
    <section class="flow-detail__section"><div class="panel__head"><div><p class="eyebrow">Audit history</p><h3>Flow event timeline</h3></div><span class="panel__note">${fmt(events.length)} Membrick-recorded events · provider telemetry is shown separately</span></div><div class="flow-timeline">${timelineHtml}</div></section>`;
  document.querySelectorAll('[data-flow-board-id]').forEach(button => button.addEventListener('click', async () => {
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

const LINK_KIND_LABELS = { board: 'Board', post: 'Post', external: 'Web', reference: 'Ref' };

function boardLabel(boardId) {
  const board = state.boards.find(item => item.board_id === boardId) || state.linkedBoards.get(boardId)?.board;
  return board ? board.title || board.key : boardId;
}

function renderBoardLinks(links = []) {
  if (!links.length) return '<span class="muted">No linked items</span>';
  return `<div class="board-links">${links.map(link => {
    const target = String(link.target || '');
    const parsed = parseLinkTarget(target);
    const navigable = parsed.boardId && parsed.boardId.startsWith('board:') && (parsed.kind === 'board' || parsed.sequence);
    let item;
    if (navigable) {
      const label = `${boardLabel(parsed.boardId)}${parsed.sequence ? ` · post ${parsed.sequence}` : ''}`;
      item = `<button type="button" class="board-link" data-board-link="${esc(parsed.boardId)}"${parsed.sequence ? ` data-post-sequence="${parsed.sequence}"` : ''}>${esc(label)}</button>`;
    } else if (parsed.kind === 'external') {
      item = `<a class="board-link" href="${esc(target)}" target="_blank" rel="noopener noreferrer">${esc(target)}</a>`;
    } else {
      item = `<span class="board-link board-link--plain">${esc(target.startsWith('repo:') ? target.slice(5) : target)}</span>`;
    }
    return `<span class="board-link-item board-link-item--${parsed.kind}"><small>${esc(link.relation || 'related')}</small><span class="board-link-kind">${LINK_KIND_LABELS[parsed.kind]}</span>${item}</span>`;
  }).join('')}</div>`;
}

// In-place navigation keeps a trail back to where the reader started.
async function navigateBoardLink(boardId, postSequence) {
  if (boardId === state.selectedBoardId) {
    if (postSequence == null) return;
    if (document.querySelector(`#post-${postSequence}`)) focusBoardPost(postSequence);
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

// Group posts only by the declared group_by field and declared values. Posts
// whose value is missing or undeclared are counted as "Other", never guessed.
function groupPosts(component, posts) {
  const matching = posts.filter(post => postMatches(component, post));
  const field = typeof component.group_by === 'string' ? component.group_by : null;
  const declared = Array.isArray(component.groups) ? component.groups.filter(group => group && typeof group === 'object' && Object.hasOwn(group, 'value')) : [];
  // A group's color comes only from a tone the template declares; otherwise it
  // gets a neutral color by position, so no meaning is read into its value.
  const groups = declared.map((group, index) => ({
    label: String(group.label ?? group.value),
    tone: GROUP_TONES.has(group.tone) ? group.tone : `n${index % 6}`,
    posts: field ? matching.filter(post => post.fields && Object.hasOwn(post.fields, field) && Object.is(post.fields[field], group.value)) : [],
  }));
  const other = matching.filter(post => !groups.some(group => group.posts.includes(post)));
  return { declared: Boolean(field) && declared.length > 0, groups, other, total: matching.length };
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
    const summary = childGroups && entry ? groupSummaryHtml(groupPosts(childGroups, entry.posts), entry.hasMore) : '';
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
      const grouping = groupPosts(component, posts);
      if (!grouping.declared) {
        // Without declared values there is nothing to group by: show a neutral list.
        return `<section class="template-section">${title}<p class="muted template-note">No groups are declared for this section; items are shown in posting order.</p>${matching.map(post => renderPostCard(post, instance, true, component.field_ids)).join('') || '<div class="empty-state">No posts in this section.</div>'}</section>`;
      }
      const groups = grouping.other.length ? [...grouping.groups, { label: 'Other or unclassified', posts: grouping.other, tone: 'other' }] : grouping.groups;
      // Empty groups collapse into one row of zero-count chips.
      const filled = groups.filter(group => group.posts.length);
      const empty = groups.filter(group => !group.posts.length);
      const emptyRow = empty.length ? `<div class="template-group-empties"><span class="muted">No items in</span>${empty.map(group => `<span class="group-chip"><i class="group-tone--${esc(group.tone)}"></i>${esc(group.label)} <b>0</b></span>`).join('')}</div>` : '';
      return `<section class="template-section template-section--groups">${title}${groupSummaryHtml(grouping, hasMorePosts)}<div class="template-groups">${filled.map(group => `<div class="template-group"><div class="template-group__head"><h4><i class="group-dot group-tone--${esc(group.tone)}"></i>${esc(group.label)}</h4><span>${fmt(group.posts.length)}${hasMorePosts ? ' loaded' : ''}</span></div>${group.posts.map(post => renderPostCard(post, instance, true, component.field_ids)).join('')}</div>`).join('')}${emptyRow}</div></section>`;
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
  const shown = ranked.slice(0, 6);
  return `<div class="board-contributors"><span class="muted">Contributors</span>${shown.map(([name, count]) => `<span class="board-contributors__item">${agentChip(name)}<b class="board-contributors__count">${fmt(count)}</b></span>`).join('')}${ranked.length > shown.length ? `<span class="muted">+${fmt(ranked.length - shown.length)} more</span>` : ''}</div>`;
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
  const links = renderBoardLinks(board.links || []);
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
  const components = Array.isArray(templateSnapshot?.presentation?.components) ? templateSnapshot.presentation.components : [];
  const layout = (await Promise.all(components.map((component, index) => renderTemplateComponent(component, index, board, posts, instance, snapshot.hasMore, usedLinked)))).join('');
  const hasHero = components.some(component => component?.kind === 'hero');
  const unknownFields = board.fields && typeof board.fields === 'object' && !Array.isArray(board.fields)
    ? Object.keys(board.fields).filter(id => !fieldIndex(templateSnapshot?.schema).has(id)) : [];
  const unknownFieldRows = instance ? fieldRows(board.fields, templateSnapshot?.schema, unknownFields) : '';
  const fallbackBoardFields = !instance ? fieldRows(board.fields, null) : '';
  // A newer render, or a different board selection, owns the panel now.
  if (renderId !== state.boardRenderSeq || generation !== state.boardGeneration || state.selectedBoardId !== board.board_id) return;
  for (const id of state.linkedBoards.keys()) if (!usedLinked.has(id)) state.linkedBoards.delete(id);
  const container = document.querySelector('#board-detail');
  const viewport = document.querySelector('.viewport');
  const sameBoard = container.dataset.boardId === board.board_id;
  const scrollTop = viewport.scrollTop;
  const openDetails = [...container.querySelectorAll('details[data-detail-id][open]')].map(item => item.dataset.detailId);
  container.dataset.boardId = board.board_id;
  const lastActivity = posts.at(-1)?.created_at;
  const postCount = `${fmt(posts.length)}${snapshot.hasMore ? '+' : ''}`;
  const typeLabel = board.type && board.type !== 'general' ? `${board.type} board` : 'Board';
  container.innerHTML = `
    ${renderBoardTrail(board)}
    <header class="board-detail__head">
      <div class="board-detail__title">
        <p class="eyebrow">${esc(typeLabel)} · <span class="mono">${esc(board.key)}</span></p>
        ${hasHero ? `<p class="board-detail__subtitle">${esc(board.title || board.key)}</p>` : `<h2>${esc(board.title || board.key)}</h2>`}
        <div class="board-stats">
          <span><b>${postCount}</b> ${posts.length === 1 && !snapshot.hasMore ? 'post' : 'posts'}</span>
          <span>Created ${timeHtml(board.created_at)}</span>
          ${lastActivity ? `<span>Last post ${timeHtml(lastActivity)}</span>` : ''}
        </div>
        ${instance ? templateProvenanceHtml(instance) : ''}
      </div>
      <div class="board-detail__badges"><span class="board-live" id="board-live" aria-live="polite">${boardLiveMarkerHtml(snapshot)}</span><span class="board-source-label">Agent-authored</span></div>
    </header>
    ${posts.length ? contributorsHtml(posts) : ''}
    ${layout ? `<div class="template-layout">${layout}</div>` : ''}
    ${unknownFieldRows ? `<section class="template-section"><h3>Additional fields</h3><p class="muted template-note">These values are not declared by the pinned template schema.</p><dl class="template-fields">${unknownFieldRows}</dl></section>` : ''}
    ${fallbackBoardFields ? `<section class="template-section"><h3>Board fields</h3><dl class="template-fields">${fallbackBoardFields}</dl></section>` : ''}
    ${(board.links || []).length ? `<section class="board-detail__links"><h3>Board links</h3>${links}</section>` : ''}
    ${layout ? `<details class="board-detail__posts board-raw-history" data-detail-id="raw-history">
      <summary><span class="eyebrow">All posts</span> <span class="muted">${postCount} in posting order, with every field</span></summary>
      <div class="board-timeline">${postHtml || '<div class="empty-state">No posts on this board yet.</div>'}</div>${more}
    </details>` : `<section class="board-detail__posts">
      <div class="board-history__head"><div><p class="eyebrow">Activity</p><h3>${postCount} ${posts.length === 1 && !snapshot.hasMore ? 'post' : 'posts'} · oldest first</h3></div>${posts.length > 3 ? `<button type="button" class="btn btn--quiet" id="board-jump-latest">Jump to latest ↓</button>` : ''}</div>
      <div class="board-timeline">${postHtml || '<div class="empty-state board-empty"><strong>No posts yet</strong><span>Agents add updates to this board with <code>board_post</code>. New posts appear here as they arrive.</span></div>'}</div>${more}
    </section>`}`;
  for (const id of openDetails) container.querySelector(`details[data-detail-id="${CSS.escape(id)}"]`)?.setAttribute('open', '');
  if (sameBoard) viewport.scrollTop = scrollTop;
  container.querySelectorAll('[data-board-link]').forEach(button => button.addEventListener('click', () => navigateBoardLink(
    button.dataset.boardLink,
    button.dataset.postSequence ? Number(button.dataset.postSequence) : null,
  )));
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
  container.querySelector('#board-jump-latest')?.addEventListener('click', () => {
    const latestSequence = posts.at(-1)?.sequence;
    if (latestSequence != null && focusBoardPost(latestSequence)) return;
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

function cycleAnalytics(events) {
  const isPrompt = event => event.prompt_preview || /^(UserPromptSubmit|UserPromptExpansion|BeforeAgent)$/i.test(event.event);
  const isStop = event => event.stop_response_preview || /^(Stop|AfterAgent|SessionEnd)$/i.test(event.event);
  const prompts = events.filter(isPrompt);
  const toolCalls = events.filter(event => event.tool_name);
  const responses = events.filter(event => event.tool_response_preview);
  const usageEvents = events.filter(event => [event.input_tokens, event.output_tokens, event.total_tokens].some(Number.isFinite));
  const sumTokens = field => usageEvents.reduce((sum, event) => sum + Number(event[field] || 0), 0);
  const durations = [];
  sessionsFor(events).forEach(session => session.events.forEach((event, index) => {
    if (!isPrompt(event)) return;
    const stop = session.events.slice(index + 1).find(isStop);
    if (stop) durations.push(Math.max(0, stop.timestamp_ms - event.timestamp_ms));
  }));
  return {
    prompts: prompts.length,
    toolCalls: toolCalls.length,
    responses: responses.length,
    stops: durations.length,
    completionRate: prompts.length ? durations.length / prompts.length : 0,
    averageTools: prompts.length ? toolCalls.length / prompts.length : 0,
    averageDuration: durations.length ? durations.reduce((sum, value) => sum + value, 0) / durations.length : 0,
    tokenUsage: tokenAccounting(usageEvents, prompts.length)
  };
}

// The three input counters are DISJOINT: `input_tokens` is the uncached
// remainder only, with cached context reported separately as cache writes and
// cache reads. Real context sent = all three summed. The provider's own
// `total_tokens` ignores both cache fields, so it is recomputed here rather
// than trusted — reading it directly under-reports a cached turn enormously.
function tokenAccounting(usageEvents, promptCount) {
  const sum = field => usageEvents.reduce((total, event) => total + Number(event[field] || 0), 0);
  const fresh = sum('input_tokens');
  const cacheWrite = sum('cache_creation_input_tokens');
  const cacheRead = sum('cache_read_input_tokens');
  const output = sum('output_tokens');
  const contextIn = fresh + cacheWrite + cacheRead;
  const total = contextIn + output;
  return {
    available: usageEvents.length > 0,
    fresh, cacheWrite, cacheRead, output, contextIn, total,
    cacheHitRate: contextIn ? cacheRead / contextIn : 0,
    averageTotal: promptCount ? total / promptCount : 0,
    reportedTotal: sum('total_tokens')
  };
}

function delegationAnalytics(events) {
  const starts = events.filter(event => event.event === 'SubagentStart');
  const stops = events.filter(event => event.event === 'SubagentStop');
  const usageByAgent = new Map();
  events.filter(event => event.event === 'TurnUsage' && (event.agent_id || event.agent_type)).forEach(event => {
    const key = event.agent_id || event.agent_type;
    const usage = usageByAgent.get(key) || { fresh: 0, cacheWrite: 0, cacheRead: 0, output: 0 };
    usage.fresh += Number(event.input_tokens || 0);
    usage.cacheWrite += Number(event.cache_creation_input_tokens || 0);
    usage.cacheRead += Number(event.cache_read_input_tokens || 0);
    usage.output += Number(event.output_tokens || 0);
    usageByAgent.set(key, usage);
  });
  const usedStops = new Set();
  const runs = starts.map(start => {
    const stopIndex = stops.findIndex((stop, index) => !usedStops.has(index)
      && (start.agent_id && stop.agent_id ? start.agent_id === stop.agent_id : stop.timestamp_ms >= start.timestamp_ms));
    const stop = stopIndex >= 0 ? stops[stopIndex] : null;
    if (stop) usedStops.add(stopIndex);
    return { start, stop, duration: stop ? Math.max(0, stop.timestamp_ms - start.timestamp_ms) : null };
  });
  const known = new Set(runs.map(run => run.start.agent_id || run.start.agent_type).filter(Boolean));
  usageByAgent.forEach((usage, key) => {
    if (!known.has(key)) runs.push({ start: { agent_id: key }, stop: null, duration: null, usageOnly: true });
  });
  const completed = runs.filter(run => run.stop);
  return {
    starts: starts.length,
    completed: completed.length,
    active: runs.filter(run => !run.stop).length,
    averageDuration: completed.length ? completed.reduce((sum, run) => sum + run.duration, 0) / completed.length : 0,
    usageByAgent,
    runs: runs.sort((a, b) => (b.start.timestamp_ms || 0) - (a.start.timestamp_ms || 0)).slice(0, 8)
  };
}

function renderCyclePanel(stats) {
  const step = (label, value, note) => `<div class="flow-step"><em>${value}</em><strong>${label}</strong><small>${note}</small></div>`;
  return `
    <section class="panel">
      <div class="panel__head">
        <div><p class="eyebrow">Agent run cycle</p><h2>Execution flow</h2></div>
        <span class="panel__note">Prompt to final response</span>
      </div>
      <div class="flow">
        ${step('Prompts', fmt(stats.prompts), 'submitted')}
        ${step('Tool work', fmt(stats.toolCalls), 'invocations')}
        ${step('Responses', fmt(stats.responses), 'returned')}
        ${step('Stops', fmt(stats.stops), 'completed')}
      </div>
      <div class="figure-row">
        <div><span>Completion rate</span><strong>${pct(stats.completionRate)}</strong></div>
        <div><span>Avg. tools / prompt</span><strong>${stats.averageTools.toFixed(1)}</strong></div>
        <div><span>Avg. cycle time</span><strong>${seconds(stats.averageDuration)}</strong></div>
      </div>
    </section>`;
}

function renderTokenPanel(stats) {
  const usage = stats.tokenUsage;
  const head = `
    <div class="panel__head">
      <div><p class="eyebrow">Token usage</p><h2>Context accounting</h2></div>
      <span class="panel__note">Cache-aware</span>
    </div>`;
  if (!usage.available) {
    return `<section class="panel">${head}<div class="empty-state">No provider token usage has been reported yet.</div></section>`;
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
      <p class="token-note">Avg. ${fmt(Math.round(usage.averageTotal))} tokens / prompt. Provider-reported total is ${fmt(usage.reportedTotal)} — it counts neither cache reads nor cache writes.</p>
    </section>`;
}

function renderDelegationPanel(events) {
  const stats = delegationAnalytics(events);
  if (!stats.starts && !stats.completed && !stats.usageByAgent.size) return '';
  const rows = stats.runs.map(run => {
    const label = run.start.agent_type || run.start.agent_id || 'Subagent';
    const id = run.start.agent_type ? run.start.agent_id || '' : '';
    const status = run.stop ? 'completed' : 'running';
    const usage = stats.usageByAgent.get(run.start.agent_id || run.start.agent_type);
    const usageTotal = usage ? usage.fresh + usage.cacheWrite + usage.cacheRead + usage.output : 0;
    return `
      <div class="delegation-row">
        <div><strong>${esc(label)}</strong><small>${esc(id || 'delegated task')} · ${clock(run.start.timestamp_ms)}${usage ? ` · ${fmt(usageTotal)} tokens` : ''}</small></div>
        <span class="delegation-duration ${status}">${run.duration == null ? 'in progress' : seconds(run.duration)}</span>
        <span class="badge ${run.stop ? 'active' : 'idle'}">${status}</span>
      </div>`;
  }).join('');
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
        <div><span>Avg. duration</span><strong>${seconds(stats.averageDuration)}</strong></div>
      </div>
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
  const stats = cycleAnalytics(events);

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
    ${renderDelegationPanel(events)}

    <div class="stat-row">
      <article class="stat"><span>Documents</span><strong>${fmt(indexes.reduce((sum, index) => sum + Number(index.source_document_count || 0), 0))}</strong><small>${fmt(indexes.length)} private memory stores</small></article>
      <article class="stat"><span>Records</span><strong>${fmt(indexes.reduce((sum, index) => sum + Number(index.record_count || 0), 0))}</strong><small>across private memory stores</small></article>
      <article class="stat"><span>Sessions</span><strong>${fmt(sessions.length)}</strong><small>${fmt(provider.sessions_active)} currently active</small></article>
      <article class="stat"><span>Tool calls</span><strong>${fmt(toolEvents.length)}</strong><small>${fmt(tools.length)} categories</small></article>
    </div>

    <div class="row row--narrow-first">
      <section class="panel">
        <div class="sub-head">
          <div><p class="eyebrow">Session history</p><h2>Sessions</h2></div>
          <span class="panel__note">${sessions.length ? `${sessions.length} observed` : 'None'}</span>
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
          <span class="panel__note">Select for details</span>
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
        <span class="panel__note">${calls.length} most recent</span>
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
