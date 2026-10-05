// Minimal app-server peer for the inactive-compaction regression. No inference.
import { appendFileSync, existsSync, readFileSync } from 'node:fs'
import { createInterface } from 'node:readline'

if (process.argv.includes('--version')) {
  console.log('codex-cli 0.144.0')
  process.exit(0)
}
if (!process.argv.includes('app-server')) process.exit(1)
const root = process.env.ORX_COMPACT_TEST_DIR
const send = (value) => console.log(JSON.stringify(value))
const mode = () => existsSync(`${root}/mode`) ? readFileSync(`${root}/mode`, 'utf8') : ''
let turn = 0
for await (const line of createInterface({ input: process.stdin })) {
  const request = JSON.parse(line)
  if (!request.method) continue
  appendFileSync(`${root}/requests.jsonl`, JSON.stringify({ ...request, pid: process.pid }) + '\n')
  const { id, method, params } = request
  if (method === 'initialized') continue
  if (method === 'initialize' && mode() === 'stall-initialize') continue
  if (method === 'thread/resume') {
    if (mode() === 'reject-resume') {
      send({ id, error: { code: -32600, message: 'fixture cannot resume saved thread' } })
    } else {
      send({ id, result: { thread: { id: mode() === 'wrong-thread' ? 'replacement' : params.threadId }, model: 'fixture-model' } })
    }
    continue
  }
  if (method === 'thread/compact/start') {
    send({ id, result: {} })
    const threadId = params.threadId
    // A stale completed turn must not settle the new compaction.
    send({ method: 'turn/completed', params: { threadId, turn: { id: 'old', status: 'completed' } } })
    const turnId = `compact-${++turn}`
    send({ method: 'turn/started', params: { threadId, turn: { id: turnId } } })
    if (mode() !== 'stall-compact') {
      send({ method: 'turn/completed', params: { threadId, turn: { id: turnId, status: 'completed' } } })
    }
    continue
  }
  send({ id, result: method === 'model/list' ? { data: [] } : {} })
}
