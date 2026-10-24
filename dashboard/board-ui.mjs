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
