import { expect, mock, test } from 'claude-code/testing'

import { embody, restore, rollBones } from './companion'
import { fleetLine, refill } from './register'
import { newsFact, summarizeTurn } from './voice'

const OLD_CONFIG = JSON.stringify({
  oauthAccount: { accountUuid: 'u-1' },
  companion: { name: 'Quip', personality: 'A sarcastic ghost who haunts your error logs.', hatchedAt: 7 },
})

function pane() {
  return {
    plugin: 'buddy',
    component: 'Pane',
    surface: 'terminal',
    requestId: 'buddy',
    viewport: { columns: 160, rows: 40 },
    props: { title: 'Quip', isFocused: false, bodyColumns: 24, placement: 'dock', scroll: { offset: 0, bodyRows: 30 }, view: {} },
  } as const
}

function band(maxRows: number) {
  return {
    plugin: 'buddy',
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
  expect(await after.find({ type: 'Text', text: /Quip: That null check does zero work\.$/ })).toBeDefined()
})

test('a shipped node in the fleet feed is told in the buddy voice', async ($, on) => {
  const { clock } = boot(on)
  on('model.complete', (_: any, e: any) => ({ value: /parser-fix shipped PR 42/.test(JSON.stringify(e)) ? { isAnswered: true, text: 'parser-fix shipped pr 42. took long enough.', usage: null } : { isAnswered: false, text: '', usage: null } }))
  const row = { ts: new Date(60_000).toISOString(), kind: 'node_shipped', node: 'parser-fix', ref: '42', title: 'PR 42' }
  on('process.run', () => ({ value: { exitCode: 0, stdout: JSON.stringify([row]), stderr: '' } }))
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })

  await clock.advance(118_000)
  const ui = await $.ui.mount(band(8))
  await clock.advance(2_000)
  await ui.unmount()

  const after = await $.ui.mount(band(8))
  expect(await after.find({ type: 'Text', text: /: parser-fix shipped pr 42\. took long enough\.$/ })).toBeDefined()
  expect(newsFact({ ts: '', kind: 'session_spawned' })).toBe(null)
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

test('a fresh buddy hatches from the egg into the original card, and any key closes it', async ($, on) => {
  const { clock } = boot(on)
  on('model.complete', (_: any, e: any) => ({ value: { isAnswered: true, text: /personality/i.test(JSON.stringify(e)) ? 'A ghost who haunts flaky tests and gloats when they pass on retry.' : '*drifts in* hello.', usage: null } }))
  on('process.run', () => ({ value: { exitCode: 1, stdout: '', stderr: '' } }))
  on('session.root', () => ({ value: '/work' }))
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })
  await $.command.run({ command: 'buddy', args: '' })
  const row = { plugin: 'buddy', component: 'Pane', requestId: 'buddy-card', surface: 'terminal', viewport: { columns: 120, rows: 40 }, props: { title: 'Quip', isFocused: true, bodyColumns: 60, placement: 'above', scroll: { offset: 0, bodyRows: 30 }, view: {} } } as const

  const egg = await $.ui.mount(row)
  expect(await egg.find({ type: 'Text', text: 'hatching a coding buddy…' })).toBeDefined()
  await egg.unmount()

  // The crack starts on the first redraw after the soul is ready, then plays its frames.
  await clock.advance(4_000)
  await (await $.ui.mount(row)).unmount()
  await clock.advance(2_000)
  const shown = await $.ui.mount(row)
  expect(await shown.find({ type: 'Text', text: /^★+ [A-Z]+$/ })).toBeDefined()
  expect(await shown.find({ type: 'Text', text: /is here · it'll chime in as you code$/ })).toBeDefined()
  expect(await shown.find({ type: 'Text', text: 'hatching a coding buddy…' })).toBeUndefined()
  // Any key closes the card, as the original's press any key did.
  await shown.input({ key: 'close', text: 'x', kind: 'change' })
  await shown.unmount()
  const gone = await $.ui.mount(row)
  expect(await gone.find({ type: 'Text', text: /chime in as you code$/ })).toBeUndefined()
  // Desktop draws an Input as a text box, so the card there closes with a button.
  await $.command.run({ command: 'buddy', args: '' })
  const desk = await $.ui.mount({ ...row, surface: 'desktop' })
  await desk.press({ key: 'close' })
  await desk.unmount()
  expect(await (await $.ui.mount({ ...row, surface: 'desktop' })).find({ type: 'Text', text: /chime in as you code$/ })).toBeUndefined()
})

test('on Desktop the buddy shows above the prompt even when it wraps the terminal status line', async ($, on) => {
  const files = new Map([['/home/u/.claude/settings.json', JSON.stringify({ model: 'opus' })]])
  boot(on, OLD_CONFIG, new Map(), files)
  on('process.run', ($: any, e: any) =>
    e.argv.join(' ') === 'fno config get state_dir'
      ? { value: { exitCode: 0, stdout: '~/.fno/\n', stderr: '' } }
      : { value: { exitCode: 1, stdout: '', stderr: '' } })
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })
  await $.command.run({ command: 'buddy', args: 'statusline' })

  const terminal = await $.ui.mount(band(3))
  expect(await terminal.find({ type: 'Text', text: /Quip/ })).toBeUndefined()
  await terminal.unmount()
  const desktop = await $.ui.mount({ ...band(3), surface: 'desktop' })
  expect(await desktop.find({ type: 'Text', text: /Quip/ })).toBeDefined()
  await desktop.unmount()
  // With room above the input, Desktop shows the full sprite and the name below it, not the one-line face.
  const roomy = await $.ui.mount({ ...band(10), surface: 'desktop' })
  expect(await roomy.find({ type: 'Text', text: 'Quip' })).toBeDefined()
  expect(await roomy.find({ type: 'Text', text: /: / })).toBeUndefined()
})
