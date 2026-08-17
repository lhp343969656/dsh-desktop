// 冒烟测试：用 bundled-runtime 真实启动 dsh web，验证就绪协议与健康检查。
//
// 用法: node scripts/smoke-runtime.mjs [runtime目录]
// 默认 runtime 目录: resources/bundled-runtime

import { spawn } from 'node:child_process'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

const rt = resolve(process.argv[2] ?? join(import.meta.dirname, '..', 'resources', 'bundled-runtime'))
const manifest = JSON.parse(readFileSync(join(rt, 'manifest.json'), 'utf8'))
const nodeBin = join(rt, process.platform === 'win32' ? 'node.exe' : 'node')
const dshBin = join(rt, manifest.entryPath)
const dshHome = mkdtempSync(join(tmpdir(), 'dsh-smoke-'))

console.log(`smoke: ${nodeBin}\n  dsh:  ${dshBin}\n  home: ${dshHome}`)

const child = spawn(nodeBin, [dshBin, '--profile', 'web', '--host', '127.0.0.1', '--port', '0'], {
  env: { ...process.env, DSH_HOME: dshHome },
  stdio: ['ignore', 'pipe', 'pipe'],
})

let stderr = ''
child.stderr.on('data', (chunk) => {
  stderr += chunk.toString()
  if (stderr.length > 4000) stderr = stderr.slice(-4000)
})

const timeoutMs = 60_000
const url = await new Promise((resolveUrl, rejectUrl) => {
  const timer = setTimeout(() => {
    child.kill()
    rejectUrl(new Error(`ready timeout; stderr:\n${stderr}`))
  }, timeoutMs)
  let buffer = ''
  child.stdout.on('data', (chunk) => {
    buffer += chunk.toString()
    for (const line of buffer.split(/\r?\n/)) {
      const trimmed = line.trim()
      if (!trimmed) continue
      if (trimmed.startsWith('{')) {
        try {
          const json = JSON.parse(trimmed)
          if (json.event === 'ready' && typeof json.url === 'string') {
            clearTimeout(timer)
            resolveUrl(json.url)
            return
          }
        } catch {
          // not json, fall through
        }
      }
      const match = trimmed.match(/http:\/\/127\.0\.0\.1:\d+/)
      if (match) {
        clearTimeout(timer)
        resolveUrl(match[0])
        return
      }
    }
    buffer = ''
  })
  child.on('exit', (code) => {
    clearTimeout(timer)
    rejectUrl(new Error(`host exited early (code=${code}); stderr:\n${stderr}`))
  })
})

console.log(`ready: ${url}`)
const res = await fetch(url)
console.log(`health: HTTP ${res.status}`)
if (res.status < 200 || res.status >= 300) {
  throw new Error(`health check failed with ${res.status}`)
}

console.log('smoke OK, shutting down host')
child.kill()
await new Promise((done) => child.on('exit', done))
rmSync(dshHome, { recursive: true, force: true })
