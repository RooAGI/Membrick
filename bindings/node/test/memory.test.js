const assert = require('node:assert/strict')
const fs = require('node:fs')
const { mkdtemp, readFile, readdir, rm, writeFile } = require('node:fs/promises')
const { spawnSync } = require('node:child_process')
const Module = require('node:module')
const os = require('node:os')
const path = require('node:path')
const { createServer } = require('node:http')
const { test } = require('node:test')

function installFakePlatformPackage() {
  const packageNames = {
    'darwin-arm64': '@rooagi/membrick-darwin-arm64',
    'darwin-x64': '@rooagi/membrick-darwin-x64',
    'linux-arm64': '@rooagi/membrick-linux-arm64-gnu',
    'linux-x64': '@rooagi/membrick-linux-x64-gnu',
    'win32-x64': '@rooagi/membrick-win32-x64-msvc',
  }
  const packageName = packageNames[`${process.platform}-${process.arch}`]
  assert.ok(packageName, `unsupported test platform: ${process.platform}-${process.arch}`)

  const repoRoot = path.resolve(__dirname, '../../..')
  const executableName = process.platform === 'win32' ? 'membrick.exe' : 'membrick'
  let executable = process.env.MEMBRICK_TEST_RESIDENT_EXECUTABLE
  if (!executable) {
    const candidate = path.join(repoRoot, 'target', 'debug', executableName)
    if (fs.existsSync(candidate)) executable = candidate
  }
  if (!executable) {
    const build = spawnSync('cargo', ['build', '--bin', 'membrick', '--features', 'agent-integrations'], {
      cwd: repoRoot,
      encoding: 'utf8',
    })
    assert.equal(build.status, 0, `could not build resident CLI:\n${build.stderr || build.stdout}`)
    executable = path.join(repoRoot, 'target', 'debug', executableName)
  }
  assert.ok(fs.existsSync(executable), `resident CLI does not exist: ${executable}`)

  const moduleRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'membrick-node-platform-'))
  const packageDirectory = path.join(moduleRoot, 'node_modules', ...packageName.split('/'))
  fs.mkdirSync(packageDirectory, { recursive: true })
  fs.writeFileSync(path.join(packageDirectory, 'package.json'), JSON.stringify({ name: packageName, version: 'test' }))
  const bundledExecutable = path.join(packageDirectory, executableName)
  if (process.platform === 'win32') {
    fs.copyFileSync(executable, bundledExecutable)
  } else {
    fs.symlinkSync(executable, bundledExecutable)
  }
  process.env.NODE_PATH = [path.join(moduleRoot, 'node_modules'), process.env.NODE_PATH]
    .filter(Boolean)
    .join(path.delimiter)
  Module._initPaths()
  process.once('exit', () => fs.rmSync(moduleRoot, { recursive: true, force: true }))
}

installFakePlatformPackage()

const { Memory, isMemoryError, version } = require('../index.js')
const { version: packageVersion } = require('../package.json')

function resolveServerSession(body) {
  return body.session_id ?? `server-derived-${body.user_id}-${body.request_id}`
}

async function waitForResidentShutdowns(logPath, expected) {
  const deadline = Date.now() + 45000
  let log = ''
  while (Date.now() < deadline) {
    try { log = await readFile(logPath, 'utf8') } catch {}
    if ((log.match(/shutdown complete; pending writes flushed/g) || []).length >= expected) return log
    await new Promise((resolve) => setTimeout(resolve, 50))
  }
  assert.fail(`resident did not shut down ${expected} times; log:\n${log}`)
}

test('reports the Rust package version and exposes the unified Memory client', () => {
  assert.equal(version(), packageVersion)
  assert.equal(typeof Memory, 'function')
  assert.equal(typeof isMemoryError, 'function')
  assert.equal('EmbeddedMemory' in require('../index.js'), false)
  assert.equal('MemoryClient' in require('../index.js'), false)
  assert.throws(() => new Memory(), (error) => error.code === 'CONFIGURATION' && isMemoryError(error))
})

test('project constructor requires an existing project and does not create state', async (t) => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'membrick-constructor-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  assert.throws(() => new Memory({ path: directory }), (error) => error.code === 'CONFIGURATION')
  assert.deepEqual(await readdir(directory), [])
})

test('Memory.init is wrapped and explicitly initializes project state', async (t) => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'membrick-init-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  Memory.init({ path: directory })
  assert.ok((await readdir(path.join(directory, '.membrick'))).includes('server.toml'))
  assert.equal(Object.hasOwn(Memory, 'init'), true)
  assert.equal(typeof Memory.init, 'function')
  for (const declarationsPath of ['../types.d.ts', '../index.d.ts']) {
    const declarations = await readFile(path.join(__dirname, declarationsPath), 'utf8')
    assert.match(declarations, /static init\(options: MemoryOptions\): Memory/)
  }
})

test('client and session leases keep resident warm, then close allows restart', async (t) => {
  const originalPath = process.env.PATH
  process.env.PATH = ''
  t.after(() => { process.env.PATH = originalPath })
  const directory = await mkdtemp(path.join(os.tmpdir(), 'membrick-idle-restart-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const client = Memory.init({ path: directory })
  t.after(() => client.close())
  const logPath = path.join(directory, '.membrick', 'runtime', 'resident.log')

  const first = await client.add(
    [{ role: 'user', content: 'Before the managed resident connection closes.' }],
    'sdk-idle-user',
    { requestId: 'node-idle-before' },
  )
  assert.equal(first.request_id, 'node-idle-before')
  await new Promise((resolve) => setTimeout(resolve, 6000))
  let log = await readFile(logPath, 'utf8')
  assert.equal((log.match(/shutdown complete; pending writes flushed/g) || []).length, 0)

  const session = client.session('sdk-idle-user', first.session_id)
  t.after(() => session.close())
  client.close()
  await new Promise((resolve) => setTimeout(resolve, 6000))
  log = await readFile(logPath, 'utf8')
  assert.equal(
    (log.match(/shutdown complete; pending writes flushed/g) || []).length,
    0,
    'closing Memory must not release a live session lease',
  )
  session.close()
  const firstShutdownLog = await waitForResidentShutdowns(logPath, 1)
  assert.match(firstShutdownLog, /no client connections remain; shutting down resident/)

  const reopened = new Memory({ path: directory })
  t.after(() => reopened.close())
  const second = await reopened.add(
    [{ role: 'user', content: 'After the same client starts a new resident.' }],
    'sdk-idle-user',
    { requestId: 'node-idle-after' },
  )
  assert.equal(second.request_id, 'node-idle-after')
  assert.equal(second.user_id, 'sdk-idle-user')
  assert.notEqual(first.session_id, second.session_id)
  reopened.close()
  const finalLog = await waitForResidentShutdowns(logPath, 2)
  assert.ok((finalLog.match(/Membrick resident IPC listening/g) || []).length >= 2)
})

test('a missing explicit resident executable fails before queue dispatch', async (t) => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'membrick-missing-cli-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const missingCli = path.join(directory, 'missing-membrick')
  const client = Memory.init({ path: directory, residentExecutable: missingCli })
  await assert.rejects(
    client.add(
      [{ role: 'user', content: 'must not be queued' }],
      'resident-executable-test',
      { requestId: 'node-missing-cli-request' },
    ),
    (error) => error.code === 'UNAVAILABLE' && isMemoryError(error),
  )
  await assert.rejects(
    readdir(path.join(directory, '.membrick', 'runtime', 'requests')),
    { code: 'ENOENT' },
  )
})

test('Memory remote calls preserve explicit and restored session identity', async (t) => {
  const requests = []
  const server = createServer(async (request, response) => {
    const chunks = []
    for await (const chunk of request) chunks.push(chunk)
    const body = JSON.parse(Buffer.concat(chunks).toString('utf8'))
    requests.push({ method: request.method, url: request.url, body })
    response.writeHead(201, { 'content-type': 'application/json' })
    response.end(JSON.stringify({
      success: true,
      request_id: body.request_id,
      user_id: body.user_id,
    session_id: resolveServerSession(body),
    }))
  })
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
  t.after(() => new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve())))

  const address = server.address()
  const client = new Memory({ baseUrl: `http://127.0.0.1:${address.port}` })
  const standalone = await client.add([{ role: 'user', content: 'standalone call' }], 'alice', { requestId: 'durable-request-1' })
  assert.equal(standalone.request_id, 'durable-request-1')
  assert.equal(standalone.user_id, 'alice')
  assert.ok(standalone.session_id.length > 0)
  const retry = await client.add([{ role: 'user', content: 'standalone call' }], 'alice', { requestId: 'durable-request-1' })
  assert.equal(retry.session_id, standalone.session_id)

  const restored = client.session('alice', 'restored-conversation')
  const continued = await restored.add([{ role: 'user', content: 'continued call' }], { requestId: 'durable-request-2' })
  assert.equal(continued.session_id, 'restored-conversation')
  assert.deepEqual(requests.map(({ body }) => [body.request_id, body.user_id, body.session_id]), [
    ['durable-request-1', 'alice', undefined],
    ['durable-request-1', 'alice', undefined],
    ['durable-request-2', 'alice', 'restored-conversation'],
  ])
})

test('Memory remote CRUD returns consistent operation shapes', async (t) => {
  const record = {
    id: 'client-memory-1', content: 'user: Alice prefers tea', role: 'user', user_id: 'alice',
    session_id: 'conversation-1', created_at: null, expires_at_ms: null,
    supersedes_id: null, metadata: {},
  }
  const server = createServer(async (request, response) => {
    const chunks = []
    for await (const chunk of request) chunks.push(chunk)
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString('utf8')) : null
    const url = new URL(request.url, 'http://127.0.0.1')
    const send = (status, value) => {
      response.writeHead(status, value === undefined ? {} : { 'content-type': 'application/json' })
      response.end(value === undefined ? '' : JSON.stringify(value))
    }

    if (request.method === 'POST' && url.pathname === '/v1/memories') {
      send(201, { success: true, request_id: body.request_id, user_id: body.user_id, session_id: resolveServerSession(body) })
    } else if (request.method === 'POST' && url.pathname === '/v1/memories/search') {
      send(200, { data: [{ ...record, score: 0.9 }] })
    } else if (request.method === 'GET' && url.pathname === '/v1/memories/client-memory-1') {
      send(200, record)
    } else if (request.method === 'GET' && url.pathname === '/v1/memories') {
      send(200, { data: [record], next_cursor: null })
    } else if (request.method === 'PATCH' && url.pathname === '/v1/memories/client-memory-1') {
      send(200, { ...record, content: `user: ${body.content}` })
    } else if (request.method === 'DELETE' && url.pathname === '/v1/memories/client-memory-1') {
      send(204)
    } else if (request.method === 'POST' && url.pathname === '/v1/memories/refresh') {
      send(204)
    } else {
      send(404, { error: `unexpected route ${request.method} ${url.pathname}` })
    }
  })
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
  t.after(() => new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve())))

  const address = server.address()
  const client = new Memory({ baseUrl: `http://127.0.0.1:${address.port}` })
  const added = await client.add([{ role: 'user', content: 'Alice prefers tea' }], 'alice', { requestId: 'crud-request' })
  assert.equal(added.request_id, 'crud-request')
  assert.equal((await client.search('Alice preferences', 'alice', { topK: 10 }))[0].id, record.id)
  assert.equal((await client.get(record.id, 'alice')).user_id, 'alice')
  assert.equal((await client.list('alice', { limit: 100 })).data[0].id, record.id)
  assert.match((await client.update(record.id, 'alice', 'Alice prefers green tea', {})).content, /green tea/)
  assert.equal(await client.delete(record.id, 'alice'), true)
  await client.refresh()
})


test('Memory errors expose stable codes and rejected HTTP status', async (t) => {
  const server = createServer(async (_request, response) => {
    response.writeHead(422, { 'content-type': 'application/json' })
    response.end(JSON.stringify({ error: 'invalid request' }))
  })
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
  t.after(() => new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve())))
  const address = server.address()
  const client = new Memory({ baseUrl: `http://127.0.0.1:${address.port}` })
  await assert.rejects(
    client.add([{ role: 'user', content: 'rejected' }], 'alice'),
    (error) => error.code === 'REJECTED' && error.status === 422,
  )
})

test('Memory distinguishes unavailable from uncertain transport outcomes', async (t) => {
  const unavailableServer = createServer()
  await new Promise((resolve) => unavailableServer.listen(0, '127.0.0.1', resolve))
  const unavailableAddress = unavailableServer.address()
  await new Promise((resolve, reject) => unavailableServer.close((error) => error ? reject(error) : resolve()))
  const unavailableClient = new Memory({ baseUrl: `http://127.0.0.1:${unavailableAddress.port}` })
  await assert.rejects(
    unavailableClient.search('query', 'alice'),
    (error) => error.code === 'UNAVAILABLE',
  )

  const uncertainServer = createServer(async (request, response) => {
    for await (const _chunk of request) { /* consume the dispatched request */ }
    response.destroy()
  })
  await new Promise((resolve) => uncertainServer.listen(0, '127.0.0.1', resolve))
  t.after(() => new Promise((resolve, reject) => uncertainServer.close((error) => error ? reject(error) : resolve())))
  const uncertainAddress = uncertainServer.address()
  const uncertainClient = new Memory({ baseUrl: `http://127.0.0.1:${uncertainAddress.port}` })
  await assert.rejects(
    uncertainClient.add([{ role: 'user', content: 'might have arrived' }], 'alice'),
    (error) => error.code === 'UNCERTAIN',
  )
})
