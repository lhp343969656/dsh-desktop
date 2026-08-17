// 构建 bundled-runtime：node 二进制 + dsh CLI 依赖闭包 + manifest.json
//
// 用法: node scripts/build-runtime.mjs
// 环境变量:
//   DSH_NPM_PACKAGE   从 npm 安装 dsh（如 "@deepseek-ai/dsh@0.1.0-rc.6"）；
//                     不设置时从本地工作区打包（DSH_SOURCE_ROOT，默认 D:/deepseek-harness）
//   DSH_SOURCE_ROOT    工作区路径（npm 模式忽略）
//   DSH_RUNTIME_OUTPUT 输出目录（默认 resources/bundled-runtime）
//   NODE_BIN           node 二进制来源（默认 process.execPath）
//
// 产物结构:
//   bundled-runtime/
//   ├── node.exe / node          # node 二进制（独立可运行）
//   ├── lib/ config/ package.json # dsh CLI
//   ├── node_modules/             # 按依赖闭包拷贝（不包含 dev/test 文件）
//   └── manifest.json             # 壳启动时校验用

import { createHash } from 'node:crypto'
import {
  chmodSync,
  copyFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { execFileSync } from 'node:child_process'
import { dirname, join, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const sourceRoot = resolve(process.env.DSH_SOURCE_ROOT ?? 'D:/deepseek-harness')
const npmPackage = process.env.DSH_NPM_PACKAGE
const outputRoot = resolve(process.env.DSH_RUNTIME_OUTPUT ?? join(projectRoot, 'resources', 'bundled-runtime'))
const stagingRoot = resolve(process.env.DSH_RUNTIME_STAGING ?? join(projectRoot, 'resources', '.runtime-staging'))
const cliRoot = join(sourceRoot, 'apps', 'cli')
const packageDirectories = new Map()
const copiedPackages = new Map()
let packageCount = 0
let cliSourceRoot = cliRoot

if (!npmPackage && !existsSync(join(cliRoot, 'lib', 'bin.js'))) {
  throw new Error(`Built dsh CLI was not found at ${join(cliRoot, 'lib', 'bin.js')}`)
}

const nodeSource = resolve(process.env.NODE_BIN ?? process.execPath)
if (!existsSync(nodeSource)) {
  throw new Error(`Node binary not found at ${nodeSource}`)
}

rmSync(stagingRoot, { recursive: true, force: true })
rmSync(outputRoot, { recursive: true, force: true })
mkdirSync(stagingRoot, { recursive: true })
mkdirSync(outputRoot, { recursive: true })

// 1) dsh CLI 来源：npm 包（推荐，CI 用）或本地工作区
if (npmPackage) {
  const npmCmd = process.platform === 'win32' ? 'npm.cmd' : 'npm'
  console.log(`installing ${npmPackage} into staging...`)
  execFileSync(npmCmd, ['install', '--prefix', stagingRoot, npmPackage, '--omit=dev', '--no-audit', '--no-fund', '--loglevel=error'], {
    stdio: 'inherit',
    shell: process.platform === 'win32',
  })
  const spec = npmPackage.split('@').filter(Boolean)
  const packageName = npmPackage.startsWith('@') ? `@${spec[0]}` : spec[0]
  cliSourceRoot = join(stagingRoot, 'node_modules', ...packageName.split('/'))
  if (!existsSync(join(cliSourceRoot, 'lib', 'bin.js'))) {
    throw new Error(`npm package ${npmPackage} does not contain lib/bin.js (found at ${cliSourceRoot})`)
  }
  copyRuntimeFiles(cliSourceRoot, stagingRoot)
} else {
  // 扫描 workspace 中所有包目录，用于解析 workspace: 依赖
  for (const topLevel of ['apps', 'packages', 'vendor', 'native']) {
    scanPackages(join(sourceRoot, topLevel))
  }
  // dsh CLI 本体（lib/ config/ package.json，不含 test 等）
  copyRuntimeFiles(cliRoot, stagingRoot)
  // 按依赖闭包拷贝 node_modules
  copyPackageTree(cliRoot, new Set())
}

// 2) 清理其他平台的二进制文件
pruneUnsupportedPlatformFiles(stagingRoot)

// 3) 校验原生模块白名单
verifyNativeModules(stagingRoot)

// 4) 拷贝 node 二进制并验证可独立运行
const nodeName = process.platform === 'win32' ? 'node.exe' : 'node'
copyFileSync(nodeSource, join(stagingRoot, nodeName))
if (process.platform === 'darwin') {
  chmodSync(join(stagingRoot, nodeName), 0o755)
}
try {
  const version = execFileSync(join(stagingRoot, nodeName), ['--version'], { encoding: 'utf8' }).trim()
  if (!/^v\d+\.\d+\.\d+$/.test(version)) {
    throw new Error(`unexpected version output: ${version}`)
  }
  console.log(`bundled node ${version} verified`)
} catch (error) {
  throw new Error(`bundled node binary is not runnable standalone: ${error.message}`)
}

// 5) 生成 manifest
const files = listFiles(stagingRoot)
  .filter((path) => !path.endsWith(nodeName))
  .map((path) => ({
    path: relative(stagingRoot, path).split(sep).join('/'),
    sha256: sha256File(path),
  }))
const manifest = {
  schemaVersion: 1,
  runtimeVersion: '0.1.0-local.1',
  dshVersion: readJson(join(cliSourceRoot, 'package.json')).version,
  nodeVersion: execFileSync(nodeSource, ['--version'], { encoding: 'utf8' }).trim().slice(1),
  platform: process.platform, // win32 | darwin
  arch: process.arch,
  channel: 'stable',
  minShellVersion: '0.1.0',
  shutdownControl: 'none',
  entryPath: 'lib/bin.js',
  files,
}
writeFileSync(join(outputRoot, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`)
writeFileSync(
  join(outputRoot, 'THIRD_PARTY_NOTICES.txt'),
  'Runtime dependencies are supplied from the DeepSeek Harness workspace. See the project notices.\n',
)

// 6) 从 staging 产出最终目录
cpSync(stagingRoot, outputRoot, { recursive: true, dereference: true })
const physicalBytes = listFiles(outputRoot).reduce((sum, path) => sum + statSync(path).size, 0)
console.log(
  JSON.stringify(
    {
      outputRoot,
      packages: packageCount,
      physicalFiles: listFiles(outputRoot).length,
      physicalBytes,
      physicalMiB: (physicalBytes / 1024 / 1024).toFixed(1),
    },
    null,
    2,
  ),
)

rmSync(stagingRoot, { recursive: true, force: true })

// ---- 以下逻辑沿用 DeepSeek Harness 桌面端既有打包方案 ----

function scanPackages(directory) {
  if (!existsSync(directory)) return
  let entries
  try {
    entries = readdirSync(directory, { withFileTypes: true })
  } catch {
    return
  }
  if (entries.some((entry) => entry.isFile() && entry.name === 'package.json')) {
    const packageJson = readJson(join(directory, 'package.json'))
    if (typeof packageJson.name === 'string') packageDirectories.set(packageJson.name, directory)
  }
  for (const entry of entries) {
    if (!entry.isDirectory() || entry.name === 'node_modules' || entry.name.startsWith('.')) continue
    scanPackages(join(directory, entry.name))
  }
}

function copyPackageTree(sourceDirectory, ancestors) {
  const source = realpathSync(sourceDirectory)
  if (ancestors.has(source)) return
  const nextAncestors = new Set(ancestors)
  nextAncestors.add(source)
  const packageJson = readJson(join(source, 'package.json'))
  for (const [name, specifier] of Object.entries({
    ...packageJson.dependencies,
    ...packageJson.optionalDependencies,
    ...packageJson.peerDependencies,
  })) {
    const dependencySource = resolveDependency(source, name, specifier)
    if (!dependencySource) continue
    const dependencyDestination = join(stagingRoot, 'node_modules', ...name.split('/'))
    const existingSource = copiedPackages.get(name)
    if (existingSource) {
      if (existingSource !== realpathSync(dependencySource)) {
        console.warn(`Runtime dependency version conflict for ${name}; keeping ${existingSource}`)
      }
      continue
    }
    mkdirSync(dirname(dependencyDestination), { recursive: true })
    copyRuntimeFiles(dependencySource, dependencyDestination)
    copiedPackages.set(name, realpathSync(dependencySource))
    copyPackageTree(dependencySource, nextAncestors)
  }
}

function resolveDependency(sourceDirectory, name, specifier) {
  if (typeof specifier === 'string' && specifier.startsWith('workspace:')) {
    const workspacePath = packageDirectories.get(name)
    return workspacePath && existsSync(join(workspacePath, 'package.json')) ? workspacePath : null
  }
  let current = sourceDirectory
  while (true) {
    const candidate = join(current, 'node_modules', ...name.split('/'))
    if (existsSync(join(candidate, 'package.json'))) return candidate
    const parent = dirname(current)
    if (parent === current) break
    current = parent
  }
  const rootCandidate = join(sourceRoot, 'node_modules', ...name.split('/'))
  return existsSync(join(rootCandidate, 'package.json')) ? rootCandidate : null
}

function copyRuntimeFiles(sourceDirectory, destinationDirectory) {
  mkdirSync(destinationDirectory, { recursive: true })
  for (const entry of readdirSync(sourceDirectory, { withFileTypes: true })) {
    const source = join(sourceDirectory, entry.name)
    if (!shouldCopy(source, entry)) continue
    cpSync(source, join(destinationDirectory, entry.name), {
      recursive: true,
      dereference: true,
      force: true,
      filter: (candidate) => shouldCopy(candidate),
    })
  }
  packageCount += 1
}

function shouldCopy(path, directoryEntry) {
  const name = directoryEntry?.name ?? path.slice(path.lastIndexOf(sep) + 1)
  if (['node_modules', 'test', 'tests', 'examples', 'docs', 'scripts', 'coverage', '.git'].includes(name)) return false
  if (/\.(?:ts|map|tsbuildinfo)$/i.test(name)) return false
  if (/^(?:tsconfig|eslint|vitest|tsdown|knip)(?:\..+)?\.json$/i.test(name)) return false
  return true
}

function pruneUnsupportedPlatformFiles(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    const normalized = path.toLowerCase().replaceAll('\\', '/')
    const ownPlatform = process.platform === 'win32' ? 'win32' : 'darwin'
    const foreignPlatforms = process.platform === 'win32' ? ['darwin', 'linux'] : ['win32', 'linux']
    const foreign = foreignPlatforms.some((p) => {
      if (p === 'darwin') return /(?:^|\/)darwin(?:[-_])/.test(normalized) || /darwin-arm64|darwin-x64/.test(normalized)
      if (p === 'win32') return /win32[-_]/.test(normalized) || /win-x64|win-arm64/.test(normalized)
      return /(?:^|\/)linux(?:[-_])/.test(normalized) || /linux-x64|linux-arm64/.test(normalized)
    })
    const unsupportedArch =
      /(?:darwin|linux|freebsd|win32[-_]arm64|win32[-_]ia32|win10-arm64|darwin[-_]arm64|darwin[-_]x64|linux[-_]arm64|linux[-_]x64)/.test(
        normalized,
      )
    const remove = ownPlatform === 'win32' ? unsupportedArch : /(?:win32|linux)/.test(normalized)
    if (remove && entry.isFile()) {
      rmSync(path, { recursive: true, force: true })
      continue
    }
    if (entry.isDirectory()) pruneUnsupportedPlatformFiles(path)
  }
}

function verifyNativeModules(directory) {
  const allowlist = [
    '/node_modules/@img/sharp-win32-x64/',
    '/node_modules/@img/sharp-darwin-x64/',
    '/node_modules/@img/sharp-darwin-arm64/',
    '/node_modules/@koromix/koffi-win32-x64/',
    '/node_modules/@koromix/koffi-darwin-x64/',
    '/node_modules/@koromix/koffi-darwin-arm64/',
    '/node_modules/node-addon-require-builtin-win32-x64-msvc/',
    '/node_modules/node-addon-require-builtin-darwin-x64/',
    '/node_modules/node-addon-require-builtin-darwin-arm64/',
    '/node_modules/node-pty/',
  ]
  const rejected = listFiles(directory).filter((path) => {
    if (!path.endsWith('.node')) return false
    const normalized = path.toLowerCase().replaceAll('\\', '/')
    return !allowlist.some((fragment) => normalized.includes(fragment))
  })
  if (rejected.length > 0) {
    throw new Error(`Bundled Runtime contains unapproved native modules:\n${rejected.join('\n')}`)
  }
}

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function listFiles(directory) {
  if (!existsSync(directory)) return []
  const result = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) result.push(...listFiles(path))
    else result.push(path)
  }
  return result
}

function sha256File(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex')
}
