import { expect, mock, test } from 'claude-code/testing'

import { embody, restore, rollBones } from './companion'
import { fleetLine, refill } from './register'
import { narrate, summarizeTurn } from './voice'

const OLD_CONFIG = JSON.stringify({
  oauthAccount: { accountUuid: 'u-1' },
  companion: { name: 'Quip', personality: 'A sarcastic ghost who haunts your error logs.', hatchedAt: 7 },
})

function pane() {
  return {
    plugin: 'fno',
    component: 'Pane',
    surface: 'terminal',
    requestId: 'buddy',
    viewport: { columns: 160, rows: 40 },
    props: { title: 'Quip', isFocused: false, bodyColumns: 24, placement: 'dock', scroll: { offset: 0, bodyRows: 30 }, view: {} },
  } as const
}

function band(maxRows: number) {
  return {
    plugin: 'fno',
    component: 'AbovePrompt',
    surface: 'terminal',
    viewport: { columns: 120, rows: 40 },
    props: { hasSurvey: false, isWorking: false, maxRows, bodyColumns: 100, scroll: { offset: 0, bodyRows: maxRows }, view: {} },
  } as const
}

// The stubs every test needs before session.start runs the mod's hook.
function boot(on: any, config = OLD_CONFIG, saved = new Map<string, unknown>(), files = new Map<string, string>()) {
  const clock = mock.clock(on)
  // Nothing after the buddy draws in the band.
  on('ui.render', () => ({ type: 'Box', props: {}, children: [] }))
  on('session.start', () => ({ cwd: '/work' }))
  on('command.register', () => ({ value: undefined }))
  on('store.get', ($: any, e: any) => ({ value: saved.get(e.key) }))
  on('store.set', ($: any, e: any) => {
    saved.set(e.key, e.value)
    return { value: undefined }
  })
  on('env.get', () => ({ value: '/home/u' }))
  on('session.id', () => ({ value: 's1' }))
  on('fs.read', ($: any, e: any) => {
    if (e.path === '/home/u/.claude.json') return { value: config }
    if (files.has(e.path)) return { value: files.get(e.path) }
    if (e.path.endsWith('/hooks/statusline.py')) return { value: '# wrapper' }
    throw new Error('ENOENT')
  })
  on('fs.write', ($: any, e: any) => {
    files.set(e.path, e.text)
    return { value: undefined }
  })
  on('ui.open', () => ({ value: undefined }))
  on('ui.close', () => ({ value: undefined }))
  return { clock, saved, files }
}

test('a seed always rolls the same buddy', () => {
  expect(rollBones('seed-1')).toEqual(rollBones('seed-1'))
  expect(rollBones('seed-1')).not.toEqual(rollBones('seed-2'))
})

test('the turn summary names the tools that ran and the errors they hit', () => {
  const summary = summarizeTurn([
    { role: 'user', text: 'old prompt', toolUses: [] },
    { role: 'user', text: 'fix the parser', toolUses: [] },
    { role: 'assistant', text: 'trying', toolUses: [{ tool: 'Edit' }, { tool: 'Bash' }], toolResults: [{ text: 'exit 1: parse error', isError: true }] },
  ])
  expect(summary).toBe('[user]: fix the parser\n[assistant]: trying\n[tools]: Edit, Bash\n[error]: exit 1: parse error')
})

test('an old buddy comes back with its name and the species its personality names', () => {
  const soul = restore(OLD_CONFIG, 0)!
  expect(soul).toMatchObject({ name: 'Quip', species: 'ghost', hatchedAt: 7, seed: 'u-1' })
  expect(embody(soul).species).toBe('ghost')
  expect(restore('{}', 0)).toBe(null)
})

test('/buddy statusline wraps the user status line, writes frames, and pane restores it exactly', async ($, on) => {
  const mine = { type: 'command', command: '~/bin/my-status', padding: 2 }
  const files = new Map([['/home/u/.claude/settings.json', JSON.stringify({ model: 'opus', statusLine: mine })]])
  const { clock } = boot(on, OLD_CONFIG, new Map(), files)
  on('process.run', ($: any, e: any) =>
    e.argv.join(' ') === 'fno config get state_dir'
      ? { value: { exitCode: 0, stdout: '~/.fno/\n', stderr: '' } }
      : { value: { exitCode: 1, stdout: '', stderr: '' } })
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })

  await $.command.run({ command: 'buddy', args: 'statusline' })
  const wrapped = JSON.parse(files.get('/home/u/.claude/settings.json')!)
  expect(wrapped.statusLine).toEqual({ type: 'command', command: 'python3 /home/u/.fno/state/buddy/statusline.py', padding: 2, refreshInterval: 1 })
  expect(wrapped.model).toBe('opus')
  expect(JSON.parse(files.get('/home/u/.fno/state/buddy/inner.json')!).statusLine).toEqual(mine)

  await clock.advance(600)
  const frame = JSON.parse(files.get('/home/u/.fno/state/buddy/frames/s1.json')!)
  expect(frame).toMatchObject({ name: 'Quip', speech: 'Quip is back. did you miss me?' })

  await $.command.run({ command: 'buddy', args: 'pane' })
  expect(JSON.parse(files.get('/home/u/.claude/settings.json')!).statusLine).toEqual(mine)
  expect(fleetLine({ live_workers: 17 },{ questions: [1, 2, 3] }, [{}, {}])).toBe('17 workers · 3 asks · 2 PRs')
  expect(fleetLine(undefined, undefined, undefined)).toBe('')
})

test('a finished turn shows the model reaction, with no canned line first', async ($, on) => {
  const { clock } = boot(on)
  on('session.messages', () => ({ value: [{ role: 'user', text: 'fix the parser', toolUses: [] }, { role: 'assistant', text: 'done', toolUses: [{ tool: 'Edit', input: {} }] }] }))
  on('model.complete', () => ({ value: { isAnswered: true, text: '"That null check does zero work."', usage: null } }))
  on('turn.complete', () => ({ text: '' }))
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })

  const ui = await $.ui.mount(band(8))
  await $.turn.complete({ turnId: 't1', answer: 'done', durationMs: 9000, isAborted: false, usage: null })
  await clock.advance(1)
  await ui.unmount()

  const after = await $.ui.mount(band(8))
  expect(await after.find({ type: 'Text', text: /Quip: that null check does zero work\.$/ })).toBeDefined()
})

test('a shipped node in the fleet feed becomes a line with no model call', async ($, on) => {
  const { clock } = boot(on)
  let modelCalls = 0
  on('model.complete', (e: any) => {
    // The once-per-buddy idle batch is its own call; the feed line must not add one.
    if (!/mutters/.test(e.prompt)) modelCalls += 1
    return { value: { isAnswered: true, text: 'no', usage: null } }
  })
  const row = { ts: new Date(60_000).toISOString(), kind: 'node_shipped', node: 'parser-fix', ref: '42', title: 'PR 42' }
  on('process.run', () => ({ value: { exitCode: 0, stdout: JSON.stringify([row]), stderr: '' } }))
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })

  await clock.advance(118_000)
  const ui = await $.ui.mount(band(8))
  await clock.advance(2_000)
  await ui.unmount()

  const after = await $.ui.mount(band(8))
  expect(await after.find({ type: 'Text', text: /: psst\. parser-fix shipped pr 42\.$/ })).toBeDefined()
  expect(modelCalls).toBe(0)
  expect(narrate({ ts: '', kind: 'session_spawned' })).toBe(null)
  // Rerolls: one a day, one per two ships counted once however many sessions read them, banked to three.
  expect(refill(undefined, 'd1').bank).toBe(1)
  expect(refill({ bank: 0, day: 'd1', ships: 0, shipAt: 0 }, 'd1', [5, 5, 9])).toEqual({ bank: 1, day: 'd1', ships: 0, shipAt: 9 })
  expect(refill({ bank: 0, day: 'd1', ships: 0, shipAt: 9 }, 'd1', [5, 9]).bank).toBe(0)
  expect(refill({ bank: 3, day: 'd1', ships: 1, shipAt: 0 }, 'd2', [1]).bank).toBe(3)
})

test('petting a short sprite puts the hearts above it, not over its head', async ($, on) => {
  const saved = new Map<string, unknown>([['soul', { seed: 's2', name: 'Pip', personality: '', hatchedAt: 0, species: 'duck' }]])
  boot(on, OLD_CONFIG, saved)
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })
  await $.command.run({ command: 'buddy', args: 'pet' })

  const ui = await $.ui.mount(pane())
  expect(await ui.find({ type: 'Text', text: /\u2665/ })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: '    __      ' })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: '♥' })).toBeDefined()
})
