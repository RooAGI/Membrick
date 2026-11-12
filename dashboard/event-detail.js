function escapeHtml(value) {
  return String(value).replace(/[&<>'"]/g, character => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', "'": '&#39;', '"': '&quot;'
  }[character]));
}

function eventTime(timestamp) {
  return timestamp ? new Date(timestamp).toLocaleTimeString() : '—';
}

function argumentObject(event) {
  if (!event.tool_arguments) return {};
  try { return JSON.parse(event.tool_arguments); } catch { return {}; }
}

export function toolGroup(name) {
  const value = String(name || 'Unknown tool');
  if (/^mcp__/i.test(value)) return 'MCP';
  if (/^(apply_patch|patch|edit|write_file|str_replace)/i.test(value)) return 'Patch';
  if (/^(bash|shell|sh|zsh|exec|terminal|run_command|rg|grep|find|ls|cat|sed|pwd|git|cargo|npm|node|curl|mkdir|cp|mv|rm)$/i.test(value)) return 'Bash';
  return value;
}

function patchText(event) {
  const args = argumentObject(event);
  return String(args.patch || args.input || args.content || args.command || '');
}

function symbolsFor(file, text) {
  const extension = file.split('.').pop()?.toLowerCase();
  const patterns = {
    js: /(?:async\s+)?function\s+([\w$]+)|(?:const|let|var)\s+([\w$]+)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[\w$]+)\s*=>|class\s+([\w$]+)/g,
    jsx: /(?:async\s+)?function\s+([\w$]+)|(?:const|let|var)\s+([\w$]+)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[\w$]+)\s*=>|class\s+([\w$]+)/g,
    ts: /(?:async\s+)?function\s+([\w$]+)|(?:const|let|var)\s+([\w$]+)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[\w$]+)\s*=>|class\s+([\w$]+)/g,
    tsx: /(?:async\s+)?function\s+([\w$]+)|(?:const|let|var)\s+([\w$]+)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[\w$]+)\s*=>|class\s+([\w$]+)/g,
    py: /(?:async\s+)?def\s+([\w$]+)|class\s+([\w$]+)/g,
    rs: /(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([\w$]+)|(?:struct|enum)\s+([\w$]+)/g,
    go: /func\s+(?:\([^)]*\)\s*)?([\w$]+)|type\s+([\w$]+)/g
  };
  const pattern = patterns[extension];
  if (!pattern) return [];
  return [...text.matchAll(pattern)].map(match => match.slice(1).find(Boolean)).filter(Boolean);
}

function patchDetails(event) {
  const text = patchText(event);
  if (!text) return [];
  const files = [...text.matchAll(/\*\*\* (Update|Add|Delete) File: ([^\n]+)/g)];
  if (!files.length) return [];
  return files.map((match, index) => {
    const file = match[2].trim();
    const body = text.slice(match.index, files[index + 1]?.index ?? text.length);
    const symbols = [...new Set(symbolsFor(file, body))].slice(0, 3);
    const hunks = [...body.matchAll(/@@ -([\d]+)(?:,([\d]+))? \+([\d]+)(?:,([\d]+))?/g)];
    const additions = body.split('\n').filter(line => line.startsWith('+') && !line.startsWith('+++')).length;
    const removals = body.split('\n').filter(line => line.startsWith('-') && !line.startsWith('---')).length;
    const lineRange = hunks.length ? `lines ${hunks[0][3]}${hunks[0][4] ? `–${Number(hunks[0][3]) + Number(hunks[0][4]) - 1}` : ''}` : null;
    const summary = symbols.length ? symbols.join(', ') : lineRange || 'file contents';
    const change = match[1].toLowerCase() === 'add' ? 'added' : match[1].toLowerCase() === 'delete' ? 'deleted' : `+${additions}/−${removals}`;
    return `${file} · ${summary} · ${change}`;
  });
}

export function toolDetail(event) {
  const name = String(event.tool_name || 'Unknown tool');
  const args = argumentObject(event);
  if (/^mcp__/i.test(name)) {
    const parts = name.split('__').filter(Boolean);
    return parts.length > 2 ? `${parts[1]} · ${parts.slice(2).join('__')}` : parts.slice(1).join('__');
  }
  if (toolGroup(name) === 'Patch') {
    const details = patchDetails(event);
    return details.length ? details.join(' | ') : name;
  }
  if (toolGroup(name) === 'Bash') {
    const command = String(args.command || args.cmd || args.script || args.input || '');
    return command.trim().split(/\s+/)[0] || name;
  }
  return name;
}

function preview(event, label, field) {
  if (!event[field]) return '';
  return `<div class="event-field"><span>${label}</span><pre>${escapeHtml(event[field])}</pre></div>`;
}

function toolSummary(event) {
  const group = toolGroup(event.tool_name);
  if (group === 'Patch') return patchDetails(event).join('\n') || 'Patch content';
  if (group === 'Bash') {
    const args = argumentObject(event);
    return String(args.command || args.cmd || args.script || args.input || toolDetail(event)).trim();
  }
  return toolDetail(event);
}

export function renderToolCall(event, variant = 'history') {
  const isMembrickSearch = /^mcp__membrick__search$/i.test(String(event.tool_name || ''));
  const response = isMembrickSearch
    ? renderMcpSearchResponse(event.tool_response_preview, event.following_calls || [], event.mcp_search_id, false, event.since_ms || 0)
    : preview(event, 'Tool response', 'tool_response_preview');
  const matchReasons = [...new Set(event.memory_match?.reasons || [])];
  const matchClass = matchReasons.length ? ' tool-call-item--memory-match' : '';
  const matchKeywords = [...new Set(event.memory_match?.keywords || [])];
  const matchBadge = matchReasons.length
    ? `<span class="tool-memory-match" title="Matched retrieved memory by ${escapeHtml(matchReasons.join(', '))}${matchKeywords.length ? `: ${escapeHtml(matchKeywords.join(', '))}` : ''}">Memory match</span>`
    : '';
  const matchDetail = matchKeywords.length
    ? `<small class="tool-memory-match-detail">Matched: ${matchKeywords.map(escapeHtml).join(' · ')}</small>`
    : '';
  const callId = event.memory_match_call_id
    ? ` data-tool-call-id="${escapeHtml(event.memory_match_call_id)}"`
    : '';
  const group = toolGroup(event.tool_name);
  const detail = toolDetail(event);
  const summaryLabel = group === 'Bash' ? 'Parsed command' : group === 'Patch' ? 'Parsed changes' : 'Parsed operation';
  const parsed = `<div class="tool-face tool-face-parsed"><span>${summaryLabel}</span><pre>${highlightMatchedText(toolSummary(event), matchKeywords)}</pre><small>Click to view raw arguments</small></div>`;
  const raw = `<div class="tool-face tool-face-raw"><span>Raw arguments</span><pre>${escapeHtml(event.tool_arguments || '{}')}</pre><small>Click to return to parsed view</small></div>`;
  const card = `<div class="tool-flip" role="button" tabindex="0" aria-label="Toggle parsed and raw tool call" data-flip-card><div class="tool-flip-inner">${parsed}${raw}</div></div>${response}`;
  if (variant === 'live') return `<div class="activity-item tool-call-item${matchClass}"${callId}><i class="activity-dot"></i><div class="event-detail"><div class="tool-heading"><strong>${escapeHtml(group)}</strong><span>${escapeHtml(detail)}</span><em>${escapeHtml(event.event)}</em>${matchBadge}</div><small>${escapeHtml(event.category || 'tool')} · ${eventTime(event.timestamp_ms)}</small>${matchDetail}${card}</div></div>`;
  return `<div class="call-row tool-call-item${matchClass}"${callId}><div class="call-main"><div><strong>${escapeHtml(group)}</strong><span class="tool-detail">${escapeHtml(detail)}</span><span class="event-chip">${escapeHtml(event.event)}</span>${matchBadge}</div><small>${escapeHtml(event.session_key || 'unknown')} · ${eventTime(event.timestamp_ms)}</small>${matchDetail}</div><div class="event-detail">${card}</div></div>`;
}

export function highlightMatchedText(value, keywords = []) {
  const text = String(value || '');
  const terms = [...new Set(keywords.map(term => String(term || '').trim()).filter(Boolean))]
    .sort((a, b) => b.length - a.length);
  if (!terms.length) return escapeHtml(text);
  const expression = new RegExp(terms.map(term => term.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|'), 'gi');
  let output = '';
  let offset = 0;
  for (const match of text.matchAll(expression)) {
    const index = match.index ?? 0;
    output += escapeHtml(text.slice(offset, index));
    output += `<mark class="tool-memory-match__term">${escapeHtml(match[0])}</mark>`;
    offset = index + match[0].length;
  }
  return output + escapeHtml(text.slice(offset));
}

function parseJson(value) {
  if (value && typeof value === 'object') return value;
  if (typeof value !== 'string') return null;
  try { return JSON.parse(value); } catch { return null; }
}

function decodedMembrickSearch(value) {
  let body = parseJson(value);
  for (let depth = 0; depth < 3 && body; depth += 1) {
    if (Array.isArray(body.results) || Array.isArray(body.data)) return body;
    const text = body.content?.find?.(item => typeof item?.text === 'string')?.text;
    const nested = parseJson(text);
    if (!nested || nested === body) break;
    body = nested;
  }
  return body;
}

export function searchResultMemories(rawResponse) {
  const body = decodedMembrickSearch(rawResponse);
  const results = Array.isArray(body?.results) ? body.results
    : Array.isArray(body?.data) ? body.data : [];
  return results;
}

export function injectedMemoriesFromEvent(event) {
  try {
    const memories = JSON.parse(event?.retrieved_memory_details || '[]');
    return Array.isArray(memories) ? memories : [];
  } catch { return []; }
}

function normalizedMatchText(value) {
  return String(value || '').normalize('NFKC').toLocaleLowerCase()
    .match(/[\p{L}\p{N}]+/gu)?.join(' ') || '';
}

function normalizedProseText(value) {
  const withoutPaths = String(value || '')
    .replace(/(?:^|[\s"'`=:(])(?:~\/|\/|\.{1,2}\/)[^\s"'`<>|;,()]+/g, ' ')
    .replace(/\b(?:[\w.-]+[\\/])+[\w.-]+\.[A-Za-z0-9]{1,10}\b/g, ' ');
  return normalizedMatchText(withoutPaths);
}

function callText(call) {
  return normalizedMatchText([
    call.pre?.tool_arguments,
    call.post?.tool_arguments,
    call.post?.tool_response_preview
  ].filter(Boolean).join('\n'));
}

function rawCallArguments(call) {
  return [call.pre?.tool_arguments, call.post?.tool_arguments]
    .map(parseJson)
    .filter(value => value && typeof value === 'object');
}

function commandSignals(call) {
  const args = rawCallArguments(call);
  const commands = args.map(value => value.command || value.cmd || value.script || value.input)
    .filter(value => typeof value === 'string').join('\n');
  const tokens = commands.match(/"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s;&|]+/g) || [];
  const searchTerms = [];
  const paths = [];
  for (let index = 0; index < tokens.length; index += 1) {
    const token = tokens[index].replace(/^["']|["']$/g, '').replace(/\\(["'\\])/g, '$1');
    const executable = token.split('/').pop()?.toLowerCase();
    if (['rg', 'grep', 'egrep', 'fgrep', 'ag', 'ack'].includes(executable)) {
      for (const candidate of tokens.slice(index + 1)) {
        const value = candidate.replace(/^["']|["']$/g, '').replace(/\\(["'\\])/g, '$1');
        if (!value || value.startsWith('-')) continue;
        if (looksLikePath(value)) continue;
        if (value.length >= 6 || /[A-Z]/.test(value)) searchTerms.push(value);
        break;
      }
    }
    if (looksLikePath(token)) paths.push(token);
  }
  return { searchTerms, paths };
}

function looksLikePath(value) {
  return /(?:^|\/|\\)(?:[^/\\]+\.[A-Za-z0-9]{1,10}|\.[^/\\]+)$/.test(value)
    || /[/\\]/.test(value);
}

function normalizedPath(value) {
  return String(value || '').normalize('NFKC').toLocaleLowerCase()
    .replace(/\\/g, '/').replace(/\/+/g, '/').replace(/^\.\//, '').replace(/\/$/, '');
}

function pathMatches(commandPath, memoryPath) {
  const command = normalizedPath(commandPath);
  const memory = normalizedPath(memoryPath);
  const isFile = value => /(?:^|\/)(?:\.[^/]+|[^/.]+\.[^/]+)$/.test(value);
  return Boolean(command && memory && (command === memory
    || command.endsWith(`/${memory}`)
    || (isFile(command) && memory.endsWith(`/${command}`))));
}

function evidenceFor(memory, calls) {
  const identifier = normalizedMatchText(memory.doc_id || '');
  const source = normalizedMatchText(memory.source || '');
  const content = String(memory.excerpt || memory.content || '');
  const matchContent = normalizedProseText(content);
  const mentionedPaths = content.match(/(?:[\w.-]+[\\/])+[\w.-]+\.[A-Za-z0-9]{1,10}/g) || [];
  const memoryPaths = [memory.source, ...(Array.isArray(memory.links) ? memory.links : []), ...mentionedPaths]
    .filter(value => typeof value === 'string');
  const words = matchContent.split(' ').filter(word => word.length >= 3);
  const phrases = [];
  for (let index = 0; index <= words.length - 4 && phrases.length < 80; index += 1) {
    phrases.push(words.slice(index, index + 4).join(' '));
  }
  const direct = [];
  const path = [];
  const query = [];
  const echoed = [];
  for (const call of calls) {
    const text = callText(call);
    const proseText = normalizedProseText([
      call.pre?.tool_arguments,
      call.post?.tool_arguments,
      call.post?.tool_response_preview
    ].filter(Boolean).join('\n'));
    const signals = commandSignals(call);
    if (signals.paths.some(commandPath => memoryPaths.some(memoryPath => pathMatches(commandPath, memoryPath)))) {
      path.push(call);
      continue;
    }
    if ((identifier && text.includes(identifier)) || (source && source.length > 8 && text.includes(source))) {
      direct.push(call);
      continue;
    }
    if (signals.searchTerms.some(term => {
      const normalized = normalizedMatchText(term);
      return normalized.length >= 6 && matchContent.includes(normalized);
    })) {
      query.push(call);
      continue;
    }
    if (phrases.some(phrase => proseText.includes(phrase))) echoed.push(call);
  }
  return { direct, path, query, echoed };
}

export function memoryMatchForToolCall(memories, call) {
  return memoryMatchDetailsForToolCall(memories, call).reasons;
}

export function memoryMatchDetailsForToolCall(memories, call) {
  const reasons = new Set();
  const keywords = new Set();
  const text = callText(call);
  for (const memory of memories) {
    const evidence = evidenceFor(memory, [call]);
    const content = normalizedProseText(memory.excerpt || memory.content || '');
    const signals = commandSignals(call);
    if (evidence.direct.length) {
      reasons.add('source or document ID');
      for (const value of [memory.doc_id, memory.source]) {
        const normalized = normalizedMatchText(value);
        if (normalized && text.includes(normalized)) keywords.add(String(value));
      }
    }
    if (evidence.path.length) {
      reasons.add('file path');
      const memoryPaths = [memory.source, ...(Array.isArray(memory.links) ? memory.links : [])]
        .filter(value => typeof value === 'string');
      const mentionedPaths = String(memory.excerpt || memory.content || '')
        .match(/(?:[\w.-]+[\\/])+[\w.-]+\.[A-Za-z0-9]{1,10}/g) || [];
      memoryPaths.push(...mentionedPaths);
      for (const value of signals.paths) {
        if (memoryPaths.some(memoryPath => pathMatches(value, memoryPath))) keywords.add(value);
      }
    }
    if (evidence.query.length) {
      reasons.add('search term');
      for (const term of signals.searchTerms) {
        const normalized = normalizedMatchText(term);
        if (normalized.length >= 6 && content.includes(normalized)) keywords.add(term);
      }
    }
    if (evidence.echoed.length) {
      reasons.add('excerpt wording');
      const proseText = normalizedProseText([
        call.pre?.tool_arguments,
        call.post?.tool_arguments,
        call.post?.tool_response_preview
      ].filter(Boolean).join('\n'));
      const words = content.split(' ').filter(word => word.length >= 3);
      for (let index = 0; index <= words.length - 4; index += 1) {
        const phrase = words.slice(index, index + 4).join(' ');
        if (proseText.includes(phrase)) keywords.add(phrase);
        if (keywords.size >= 20) break;
      }
    }
  }
  return { reasons: [...reasons], keywords: [...keywords].slice(0, 20) };
}

export function memoryMatchesForSources(calls, sources) {
  const matches = new Map();
  const ordered = sources.slice().sort((a, b) => a.at - b.at);
  ordered.forEach((source, index) => {
    const nextSourceAt = ordered.slice(index + 1).find(next => next.at > source.at)?.at ?? Infinity;
    if (!source.memories?.length) return;
    const eligibleCalls = calls.filter(call => call.startMs > source.at
      && call.startMs < nextSourceAt
      && !/^mcp__membrick__search$/i.test(call.tool_name || ''));
    for (const call of eligibleCalls) {
      const previous = matches.get(call) || { reasons: [], keywords: [] };
      const matched = memoryMatchDetailsForToolCall(source.memories, call);
      const reasons = new Set([...previous.reasons, ...matched.reasons]);
      const keywords = new Set([...previous.keywords, ...matched.keywords]);
      if (reasons.size) matches.set(call, { reasons: [...reasons], keywords: [...keywords].slice(0, 20) });
    }
  });
  return matches;
}

function measuredToolUseHtml(memories, calls, sinceMs) {
  const directCalls = new Set();
  const pathCalls = new Set();
  const queryCalls = new Set();
  const echoedCalls = new Set();
  memories.forEach(memory => {
    const evidence = evidenceFor(memory, calls);
    evidence.direct.forEach(call => directCalls.add(call));
    evidence.path.forEach(call => pathCalls.add(call));
    evidence.query.forEach(call => queryCalls.add(call));
    evidence.echoed.forEach(call => echoedCalls.add(call));
  });
  const failed = calls.filter(call => call.failed).length;
  const knownDurations = calls.filter(call => call.duration != null).map(call => call.duration);
  const elapsedMs = Math.max(...calls.map(call => call.endMs ?? call.startMs)) - sinceMs;
  const groups = [...new Set(calls.map(call => toolDetail(call.pre || call.post)))];
  return `<div class="memory-impact">
    <strong>Measured downstream tool use</strong>
    <div class="memory-impact__metrics">
      <span>${calls.length} follow-on ${calls.length === 1 ? 'call' : 'calls'}</span>
      <span>${directCalls.size} with a source or document ID reference</span>
      <span>${pathCalls.size} with a matching file path</span>
      <span>${queryCalls.size} with a matching search term</span>
      <span>${echoedCalls.size} with excerpt wording echoed</span>
      <span>${failed} failed</span>
      ${knownDurations.length ? `<span>${(knownDurations.reduce((sum, value) => sum + value, 0) / knownDurations.length / 1000).toFixed(1)}s avg. call time</span>` : ''}
      ${Number.isFinite(elapsedMs) ? `<span>${(Math.max(0, elapsedMs) / 1000).toFixed(1)}s elapsed</span>` : ''}
    </div>
    <small>Observed after this retrieval in the same turn. Excerpt wording matches when a call contains four consecutive words from retrieved text. Matches show an association; timing alone does not establish causation. Counts use retained tool events.</small>
    <div class="memory-impact__tools">${groups.length ? `Tools: ${groups.map(escapeHtml).join(' · ')}` : 'No follow-on tool calls observed in this window.'}</div>
  </div>`;
}

export function renderMcpSearchResponse(rawResponse, followingCalls = [], searchId = null, fullLoaded = false, sinceMs = 0) {
  if (!rawResponse && !searchId) return '';
  rawResponse ||= '';
  const body = decodedMembrickSearch(rawResponse);
  const results = Array.isArray(body?.results) ? body.results
    : Array.isArray(body?.data) ? body.data : null;
  if (!results) {
    return `<div class="mcp-search-response"${searchId ? ` data-mcp-search-id="${escapeHtml(searchId)}"` : ''}${fullLoaded ? ' data-full-loaded="1"' : ''}><div class="event-field"><span>Tool response</span><pre>${escapeHtml(rawResponse)}</pre></div></div>`;
  }
  const cards = results.map((result, index) => {
    const metadata = [
      result.document_type || result.type,
      result.source,
      Number.isFinite(Number(result.score)) ? `score ${Number(result.score).toFixed(3)}` : null,
      result.created_at
    ].filter(Boolean).join(' · ');
    return `<article class="mcp-search-result">
      <div class="mcp-search-result__head"><strong>#${index + 1} ${escapeHtml(result.doc_id || result.concept || 'Search result')}</strong>${result.score != null ? `<span>${escapeHtml(Number(result.score).toFixed(3))}</span>` : ''}</div>
      ${metadata ? `<small>${escapeHtml(metadata)}</small>` : ''}
      ${result.content ? `<pre>${escapeHtml(result.content)}</pre>` : ''}
    </article>`;
  }).join('');
  const impact = measuredToolUseHtml(results, followingCalls, sinceMs || followingCalls[0]?.startMs || 0);
  return `<div class="mcp-search-response"${searchId ? ` data-mcp-search-id="${escapeHtml(searchId)}"` : ''}${fullLoaded ? ' data-full-loaded="1"' : ''}>
    <div class="mcp-search-response__head"><strong>${results.length} ${results.length === 1 ? 'result' : 'results'}</strong>${body.query ? `<span>${escapeHtml(body.query)}</span>` : ''}</div>
    <div class="mcp-search-results">${cards}</div>
    ${impact}
    <details class="mcp-search-raw"><summary>Full response JSON</summary><pre>${escapeHtml(rawResponse)}</pre></details>
  </div>`;
}

export function renderEventDetail(event, variant = 'live', options = {}) {
  if (event.retrieved_memory_details) return renderMemoryRetrieval(event, variant, options.followingCalls || [], options.sinceMs);
  if (event.tool_name) return renderToolCall(event, variant === 'live' ? 'live' : 'history');
  if (event.event === 'TurnUsage') return renderTurnUsage(event);
  const detail = [
    preview(event, 'Prompt', 'prompt_preview'),
    preview(event, 'Tool response', 'tool_response_preview'),
    preview(event, 'Final response', 'stop_response_preview')
  ].join('');
  return `<div class="activity-item"><i class="activity-dot"></i><div class="event-detail"><strong>${escapeHtml(event.event || 'Lifecycle event')}</strong><small>${escapeHtml(event.category || 'lifecycle')} · ${eventTime(event.timestamp_ms)}</small><div class="event-previews">${detail}</div></div></div>`;
}

function renderTurnUsage(event) {
  const value = field => typeof event[field] === 'number' && Number.isFinite(event[field]) && event[field] >= 0
    ? event[field]
    : null;
  const fields = [
    ['input_tokens', 'Uncached input'],
    ['cache_creation_input_tokens', 'Cache write'],
    ['cache_read_input_tokens', 'Cache read'],
    ['output_tokens', 'Output']
  ];
  const parts = fields.map(([field, label]) => ({ label, amount: value(field) })).filter(part => part.amount != null);
  const reported = value('total_tokens');
  const total = parts.length ? parts.reduce((sum, part) => sum + part.amount, 0) : reported;
  const metrics = parts.length
    ? parts.map(part => `<div class="turn-usage__metric"><span>${part.label}</span><strong>${part.amount.toLocaleString()}</strong></div>`).join('')
    : `<div class="turn-usage__metric"><span>Provider-reported total</span><strong>${reported == null ? '—' : reported.toLocaleString()}</strong></div>`;
  const turn = event.turn_id ? `Turn ${escapeHtml(event.turn_id)}` : 'Turn';
  const note = parts.length && reported != null && reported !== total
    ? `<small class="turn-usage__note">Provider-reported total: ${reported.toLocaleString()}; displayed total sums input, cache, and output counts.</small>`
    : '';
  return `<div class="activity-item turn-usage-event"><i class="activity-dot"></i><div class="event-detail">
    <div class="tool-heading"><strong>Turn token usage</strong><span>${turn}</span><em>TurnUsage</em></div>
    <div class="turn-usage-event__content">
      <div class="turn-usage__total"><strong>${total == null ? '—' : total.toLocaleString()}</strong><span>tokens this turn</span></div>
      <div class="turn-usage__metrics">${metrics}</div>
    </div>
    <small>${escapeHtml(event.category || 'lifecycle')} · ${eventTime(event.timestamp_ms)}</small>${note}
  </div></div>`;
}

function renderMemoryRetrieval(event, variant, followingCalls = [], sinceMs = Number(event.timestamp_ms) || 0) {
  let memories = [];
  let detailsParsed = false;
  try {
    const parsed = JSON.parse(event.retrieved_memory_details);
    detailsParsed = Array.isArray(parsed);
    memories = detailsParsed ? parsed : [];
  } catch { memories = []; }
  const rows = memories.map((memory, index) => {
    const evidence = evidenceFor(memory, followingCalls);
    const matches = [
      ...evidence.direct.map(call => `${toolDetail(call.pre || call.post)} · ID/source`),
      ...evidence.path.map(call => `${toolDetail(call.pre || call.post)} · file path`),
      ...evidence.query.map(call => `${toolDetail(call.pre || call.post)} · search term`),
      ...evidence.echoed.map(call => `${toolDetail(call.pre || call.post)} · excerpt`)
    ];
    return `<article class="retrieval-memory"><div><span class="retrieval-rank">#${index + 1}</span><strong>${escapeHtml(memory.type || 'memory')}</strong><small>${escapeHtml(memory.source || 'unknown source')}${memory.doc_id ? ` · ${escapeHtml(memory.doc_id)}` : ''}</small></div><span class="retrieval-score">${Number(memory.score || 0).toFixed(2)}</span><pre>${escapeHtml(memory.excerpt || '')}</pre>${followingCalls.length ? `<small class="mcp-search-result__link">${matches.length ? `Matched in ${matches.map(escapeHtml).join(', ')}` : 'No text, search term, or file path match in observed follow-on calls'}</small>` : ''}</article>`;
  }).join('');
  const count = Number(event.retrieved_memory_count ?? memories.length);
  const empty = count > 0 && !detailsParsed
    ? `${count} memories were injected, but their details were truncated in this saved event.`
    : count > 0 && !memories.length
      ? 'Memory details are unavailable for this event.'
      : 'No memories were selected.';
  const body = `<div class="retrieval-meta"><span>Query</span><strong>${escapeHtml(event.retrieval_query_preview || '—')}</strong><span>${count} injected · ${event.retrieval_latency_ms || 0} ms</span></div><div class="retrieval-list">${rows || `<div class="empty-state">${escapeHtml(empty)}</div>`}</div>${measuredToolUseHtml(memories, followingCalls, sinceMs)}`;
  return `<div class="${variant === 'live' ? 'activity-item' : 'call-row'} retrieval-item"><i class="activity-dot"></i><div class="event-detail"><div class="tool-heading"><strong>Retrieved memory</strong><span>${escapeHtml(event.event)}</span><em>${escapeHtml(event.category || 'retrieval')}</em></div><small>${eventTime(event.timestamp_ms)}</small>${body}</div></div>`;
}
