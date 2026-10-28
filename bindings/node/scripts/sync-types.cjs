const fs = require('node:fs')
fs.copyFileSync('types.d.ts', 'index.d.ts')

// The generated loader exports the native addon object. Keep the small JS
// error decorator here so it is restored after every `napi build` generation.
const entryPath = 'index.js'
const marker = '// membrick:error-codes:start'
let entry = fs.readFileSync(entryPath, 'utf8')
if (!entry.includes(marker)) {
  entry += `\n\n${marker}
const __memoryErrorCodes = new Set(['REJECTED', 'UNCERTAIN', 'UNAVAILABLE', 'CONFIGURATION'])
function __decorateMemoryError(error) {
  if (!error || typeof error.message !== 'string') return error
  const match = /^\\[MEMBRICK_ERROR code=(REJECTED|UNCERTAIN|UNAVAILABLE|CONFIGURATION)(?: status=(\\d+))?\\]\\s*/.exec(error.message)
  if (!match) return error
  error.code = match[1]
  if (match[2] !== undefined) error.status = Number(match[2])
  error.message = error.message.slice(match[0].length)
  return error
}
function __wrapMemoryMethod(prototype, name) {
  const original = prototype && prototype[name]
  if (typeof original !== 'function') return
  prototype[name] = function (...args) {
    try {
      return Promise.resolve(original.apply(this, args)).catch((error) => { throw __decorateMemoryError(error) })
    } catch (error) {
      throw __decorateMemoryError(error)
    }
  }
}
for (const name of ['add', 'search', 'get', 'list', 'update', 'delete', 'refresh']) {
  __wrapMemoryMethod(module.exports.Memory && module.exports.Memory.prototype, name)
  __wrapMemoryMethod(module.exports.MemorySession && module.exports.MemorySession.prototype, name)
}
const __nativeMemory = module.exports.Memory
function __bundledResidentExecutable() {
  let packageName
  if (process.platform === 'darwin') {
    if (process.arch === 'arm64') packageName = '@rooagi/membrick-darwin-arm64'
    if (process.arch === 'x64') packageName = '@rooagi/membrick-darwin-x64'
  } else if (process.platform === 'linux') {
    if (process.arch === 'arm64') packageName = '@rooagi/membrick-linux-arm64-gnu'
    if (process.arch === 'x64') packageName = '@rooagi/membrick-linux-x64-gnu'
  } else if (process.platform === 'win32' && process.arch === 'x64') {
    packageName = '@rooagi/membrick-win32-x64-msvc'
  }
  if (!packageName) return undefined
  try {
    const packageJson = require.resolve(packageName + '/package.json')
    const executable = require('node:path').join(require('node:path').dirname(packageJson), process.platform === 'win32' ? 'membrick.exe' : 'membrick')
    return require('node:fs').existsSync(executable) ? executable : undefined
  } catch {
    return undefined
  }
}
function __withResidentExecutable(args) {
  const options = args[0]
  if (!options || typeof options !== 'object' || !options.path || options.residentExecutable) return args
  const residentExecutable = __bundledResidentExecutable()
  return [{ ...options, residentExecutable }, ...args.slice(1)]
}
if (__nativeMemory) {
  function Memory(...args) {
    if (!new.target) throw new TypeError('Memory must be constructed with new')
    try { return Reflect.construct(__nativeMemory, __withResidentExecutable(args), new.target) }
    catch (error) { throw __decorateMemoryError(error) }
  }
  Memory.prototype = __nativeMemory.prototype
  Object.setPrototypeOf(Memory, __nativeMemory)
  if (typeof __nativeMemory.init === 'function') {
    Object.defineProperty(Memory, 'init', {
      value: function (options) {
        try {
          return __nativeMemory.init(...__withResidentExecutable([options]))
        } catch (error) {
          throw __decorateMemoryError(error)
        }
      },
      writable: true,
      configurable: true,
      enumerable: true,
    })
  }
  module.exports.Memory = Memory
}
module.exports.isMemoryError = (value) =>
  value instanceof Error && __memoryErrorCodes.has(value.code)
`
  fs.writeFileSync(entryPath, entry)
}
