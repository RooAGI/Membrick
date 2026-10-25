---
hide:
  - navigation
  - toc
---

<div class="hero-stack">
<section class="hero">
  <div class="hero__eyebrow">THE MEMORY LAYER FOR AI AGENTS</div>
  <h1>Give your AI a memory that lasts.</h1>
  <p class="hero__lede"><strong>8 agent harnesses, 1 shared project memory.</strong><br><strong>No embedding, database, or server setup.</strong><br><strong>75% fewer token usage.</strong></p>
  <p><a class="md-button md-button--primary" href="quickstart/">Get started</a> <a class="md-button" href="cli/">CLI reference</a> <a class="md-button" href="agent-memory/">Explore agent memory</a> <a class="md-button" href="faq/">Questions &amp; Answers</a></p>
</section>
</div>

<section class="why-membrick" aria-labelledby="why-membrick-title">
  <header class="why-membrick__heading">
    <p class="why-membrick__eyebrow">WHY MEMBRICK</p>
    <h2 id="why-membrick-title">Memory that knows what’s still true.</h2>
    <p>Find relevant context, track changes, and see the evidence behind it.</p>
  </header>

  <div class="memory-comparison-scroll" role="region" aria-label="Ordinary retrieval compared with Membrick" tabindex="0">
    <table class="memory-comparison">
      <thead>
        <tr><th scope="col">Ordinary retrieval</th><th scope="col">Membrick</th></tr>
      </thead>
      <tbody>
        <tr><td>Finds topically similar text</td><td>Ranks relevant and current evidence</td></tr>
        <tr><td>Can surface stale decisions as if they are current</td><td>Tracks time and supersession</td></tr>
        <tr><td>Treats relative time as mostly query text</td><td>Resolves temporal intent against a reference clock</td></tr>
        <tr><td>Returns passages</td><td>Returns context with source and relationship evidence</td></tr>
        <tr><td>Treats documents mostly in isolation</td><td>Connects facts, entities, links, symbols, ownership, and time</td></tr>
        <tr><td>Leaves corpus drift hidden</td><td>Surfaces contradictions, stale claims, terminology drift, orphan pages, and missing links</td></tr>
      </tbody>
    </table>
  </div>

  <aside class="memory-history-note">
    <span aria-hidden="true">↺</span>
    <p><strong>History stays available.</strong> A historical query can still
    retrieve superseded information as history rather than presenting it as
    current state.</p>
  </aside>
</section>

## Supported Agent Harnesses {.landing-heading}

Use the same project memory inside supported agent tools. Information captured
in one can be available to the others, so people can keep using the tools they
already know.

<div class="integration-grid">
  <a href="claude-code/"><strong>Claude Code</strong><span>Memory and lifecycle hooks →</span></a>
  <a href="codex/"><strong>Codex</strong><span>Memory, hooks, and MCP →</span></a>
  <a href="gemini-cli/"><strong>Gemini CLI</strong><span>Memory and lifecycle integration →</span></a>
  <a href="agents/"><strong>More integrations</strong><span>AGY, Muse Code, OpenClaw, Hermes, and RooAGI AgentFlow →</span></a>
</div>

## Build for production {.landing-heading}

Membrick keeps memory in a persistent store and provides a shared service for
agents and applications. It supports user and session scoping, durable writes,
and retrieval that can account for newer corrections while preserving history.
Run it locally or connect through its HTTP and MCP servers. A dashboard helps
you inspect memory and agent activity. Bundled Rust integrations communicate
with their resident memory owner over local IPC; see the
[transport architecture](local-ipc-architecture.md).

<!-- BEGIN GENERATED LONGMEMEVAL-S 500-QUESTION HEADLINE -->
<section class="proof-grid proof-grid--six" aria-label="Membrick production results">
  <div><strong>79%</strong><span>fewer tokens · Claude Code</span></div>
  <div><strong>89.78%</strong><span>fractional Recall@5</span></div>
  <div><strong>98.8%</strong><span>any-hit Recall@10</span></div>
  <div><strong>88.85%</strong><span>MRR</span></div>
  <div><strong>87.91%</strong><span>NDCG@10</span></div>
  <div><strong>3.48 ms</strong><span>mean search time</span></div>
</section>
<p class="benchmark-note">LongMemEval-S · 500 questions · 2026-10-16 · <a href="benchmark-results/">Methods</a> · <a href="performance/">Agent performance</a>.</p>
<!-- END GENERATED LONGMEMEVAL-S 500-QUESTION HEADLINE -->

<section class="final-cta">
  <p>Context that persists.</p>
  <h2>Build agents that remember and improve over time.</h2>
  <a class="md-button md-button--primary" href="quickstart/">Read the quickstart</a>
</section>
