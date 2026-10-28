# Membrick for Node.js

`membrick` includes a Node.js binding for the shared Rust `Memory`. Use
`Memory` for project and remote connections:

- `Memory({ path })` connects to an existing project's resident; `baseUrl`
  selects a remote HTTP server. The constructor requires one of these options
  and never defaults to the current working directory.

## Install

```sh
npm install membrick
```

This installs the Node.js `Memory` binding and the `membrick` CLI
on supported platforms. For example:

```sh
npm install --global membrick
membrick install codex .
```

The CLI is the Rust release executable. Platform packages contain it alongside
the N-API addon, so agent integration behavior stays in the Rust CLI.

Prebuilt native packages are selected for macOS arm64/x64, Linux arm64/x64
glibc, and Windows x64. Building from a checkout requires Rust 1.88+ and
Node.js 20.17+, 22.13+, or 23.5+ for the N-API build CLI.

The package is published from the matching `v<version>` Git tag after all
platform builds succeed. The repository needs an `NPM_TOKEN` Actions secret
with permission to publish `membrick` and its `@rooagi` platform packages.

## Unified client

```js
import { Memory } from 'membrick'

const memory = new Memory({ path: process.cwd() }) // resident for this project
const receipt = await memory.add(
  [{ role: 'user', content: 'I prefer oat milk.' }],
  'alice',
)
const hits = await memory.search('What are Alice’s preferences?', 'alice')
```

Set `path` to select a project's resident, or `baseUrl` and optional
`apiKey` for a remote server. Call `Memory.init({ path })` only when the
application intends to initialize missing project state. `requestId` may be provided to `add` when an
application persists idempotency keys across retries. Use
`memory.session(userId, sessionId)` to pin identities across calls.

## Remote service

```js
const memory = new Memory({
  baseUrl: 'http://127.0.0.1:8080',
  apiKey: process.env.MEMBRICK_API_KEY,
})
const hits = await memory.search('When is the launch?', 'alice')
```


## Build from source

```sh
cd bindings/node
npm install
npm run build
npm test
```
