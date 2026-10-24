import assert from 'node:assert/strict';
import test from 'node:test';
import { escapeHtml, mergeBoardPosts, parseBoardLinkTarget } from './board-ui.mjs';

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

test('escapes agent-authored text before the dashboard inserts it as HTML', () => {
  assert.equal(escapeHtml(`<script a='"'>&`), '&lt;script a=&#39;&quot;&#39;&gt;&amp;');
});
