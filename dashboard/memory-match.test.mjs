import assert from 'node:assert/strict';
import test from 'node:test';
import {
  highlightMatchedText,
  injectedMemoriesFromEvent,
  memoryMatchDetailsForToolCall,
  memoryMatchesForSources,
  renderToolCall,
  searchResultMemories
} from './event-detail.js';

function call(id, startMs, tool_name, args) {
  const tool_arguments = JSON.stringify(args);
  return {
    tool_name,
    startMs,
    endMs: startMs + 1,
    pre: { event_id: `${id}-pre`, event: 'PreToolUse', tool_name, tool_arguments },
    post: { event_id: `${id}-post`, event: 'PostToolUse', tool_name, tool_arguments }
  };
}

test('search results and injected memory details parse from their event payloads', () => {
  const result = { doc_id: 'memory-1', excerpt: 'Retry policy' };
  assert.deepEqual(searchResultMemories(JSON.stringify({ results: [result] })), [result]);
  assert.deepEqual(searchResultMemories(JSON.stringify({ content: [{ text: JSON.stringify({ results: [result] }) }] })), [result]);
  assert.deepEqual(injectedMemoriesFromEvent({ retrieved_memory_details: JSON.stringify([result]) }), [result]);
  assert.deepEqual(injectedMemoriesFromEvent({ retrieved_memory_details: 'truncated' }), []);
});

test('matches a search term and returns the term for the card label', () => {
  const memory = { doc_id: 'memory-2', excerpt: 'The cobalt index supports exact retrieval.' };
  const toolCall = call('search-term', 2, 'Bash', { command: 'rg "cobalt index" src' });
  assert.deepEqual(memoryMatchDetailsForToolCall([memory], toolCall), {
    reasons: ['search term'],
    keywords: ['cobalt index']
  });
});

test('matches the filename and does not treat a shared workspace directory as a file match', () => {
  const memory = { source: '/Users/louis/sources/Membrick/docs/guide.md', excerpt: 'Guide details' };
  const fileCall = call('file', 2, 'Bash', { command: 'cat docs/guide.md' });
  assert.deepEqual(memoryMatchDetailsForToolCall([memory], fileCall), {
    reasons: ['file path'],
    keywords: ['docs/guide.md']
  });

  const directoryCall = call('directory', 3, 'Bash', { command: 'echo /Users/louis/sources/Membrick' });
  assert.deepEqual(memoryMatchDetailsForToolCall([memory], directoryCall), { reasons: [], keywords: [] });
});

test('does not match workspace path fragments as echoed excerpt wording', () => {
  const memory = {
    doc_id: 'memory-3',
    excerpt: 'See /Users/louis/sources/Membrick/docs/guide.md for retry policy details.'
  };
  const toolCall = call('path-fragment', 2, 'Bash', { command: 'echo /Users/louis/sources/Membrick' });
  assert.deepEqual(memoryMatchDetailsForToolCall([memory], toolCall), { reasons: [], keywords: [] });
});

test('preserves slash-separated terms such as Build/Release during excerpt matching', () => {
  const memory = { doc_id: 'memory-build-release', excerpt: 'The Build/Release pipeline retries builds three times.' };
  const toolCall = call('build-release', 2, 'Bash', { command: 'echo "Build/Release pipeline retries builds"' });
  const match = memoryMatchDetailsForToolCall([memory], toolCall);
  assert.ok(match.reasons.includes('excerpt wording'));
});

test('combines injected and search memories across compaction while excluding the search call itself', () => {
  const injectedUse = call('injected-use', 2, 'Bash', { command: 'cat docs/injected.md' });
  const search = call('search', 3, 'mcp__membrick__search', { query: 'cobalt index' });
  const searchUse = call('search-use', 5, 'Bash', { command: 'rg "cobalt index" src' });
  const postCompactionUse = call('post-compact-use', 8, 'Bash', { command: 'rg "cobalt index" src' });
  const sources = [
    { at: 1, memories: [{ source: '/workspace/docs/injected.md' }] },
    { at: 4, memories: [{ doc_id: 'memory-4', excerpt: 'The cobalt index supports retrieval.' }], call: search }
  ];

  const matches = memoryMatchesForSources([injectedUse, search, searchUse, postCompactionUse], sources);
  assert.deepEqual(matches.get(injectedUse), { reasons: ['file path'], keywords: ['docs/injected.md'] });
  assert.equal(matches.has(search), false);
  assert.deepEqual(matches.get(searchUse), { reasons: ['search term'], keywords: ['cobalt index'] });
  assert.deepEqual(matches.get(postCompactionUse), { reasons: ['search term'], keywords: ['cobalt index'] });
});

test('parsed tool card displays and highlights the matched filename', () => {
  const html = renderToolCall({
    event: 'PostToolUse',
    tool_name: 'Bash',
    tool_arguments: JSON.stringify({ command: 'cat docs/guide.md' }),
    memory_match: { reasons: ['file path'], keywords: ['docs/guide.md'] },
    memory_match_call_id: 'call-1'
  });
  assert.match(html, /Matched: docs\/guide\.md/);
  assert.match(html, /<mark class="tool-memory-match__term">docs\/guide\.md<\/mark>/);
  assert.match(html, /tool-call-item--memory-match/);
});

test('matched text is HTML-escaped while keeping the matched term highlighted', () => {
  assert.equal(
    highlightMatchedText('<script> retry policy', ['retry policy']),
    '&lt;script&gt; <mark class="tool-memory-match__term">retry policy</mark>'
  );
});
