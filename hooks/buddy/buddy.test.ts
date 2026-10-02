import { expect, mock, test } from 'claude-code/testing'

import { embody, restore, rollBones } from './companion'
import { narrate } from './voice'

const OLD_CONFIG = JSON.stringify({
  oauthAccount: { accountUuid: 'u-1' },
  companion: { name: 'Quip', personality: 'A sarcastic ghost who haunts your error logs.', hatchedAt: 7 },
})

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
function boot(on: any, config = OLD_CONFIG) {
  const clock = mock.clock(on)
  const saved = new Map<string, unknown>()
  on('session.start', () => ({ cwd: '/work' }))
  on('command.register', () => ({ value: undefined }))
  on('store.get', ($: any, e: any) => ({ value: saved.get(e.key) }))
  on('store.set', ($: any, e: any) => {
    saved.set(e.key, e.value)
    return { value: undefined }
  })
  on('env.get', () => ({ value: '/home/u' }))
  on('fs.read', () => ({ value: config }))
  return { clock, saved }
}

test('a seed always rolls the same buddy', () => {
  expect(rollBones('seed-1')).toEqual(rollBones('seed-1'))
  expect(rollBones('seed-1')).not.toEqual(rollBones('seed-2'))
})

test('an old buddy comes back with its name and the species its personality names', () => {
  const soul = restore(OLD_CONFIG, 0)!
  expect(soul).toMatchObject({ name: 'Quip', species: 'ghost', hatchedAt: 7, seed: 'u-1' })
  expect(embody(soul).species).toBe('ghost')
  expect(restore('{}', 0)).toBe(null)
})

test('the band draws the sprite when it has room and one face line when it does not', async ($, on) => {
  boot(on)
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })

  const tall = await $.ui.mount(band(8))
  expect(await tall.find({ type: 'Text', text: /\.----\./ })).toBeDefined()
  expect(await tall.find({ type: 'Text', text: 'Quip is back. did you miss me?' })).toBeDefined()
  await tall.unmount()

  const short = await $.ui.mount(band(2))
  expect(await short.find({ type: 'Text', text: /^\/.+\\ Quip: Quip is back/ })).toBeDefined()
  await short.unmount()
})

test('a finished turn shows a quick line, then the model reaction', async ($, on) => {
  const { clock } = boot(on)
  on('session.messages', () => ({ value: [{ role: 'user', text: 'fix the parser', toolUses: [] }, { role: 'assistant', text: 'done', toolUses: [{ name: 'Edit' }] }] }))
  on('model.complete', () => ({ value: { isAnswered: true, text: '"That null check does zero work."', usage: null } }))
  on('turn.complete', () => ({ text: '' }))
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })

  const ui = await $.ui.mount(band(8))
  await $.turn.complete({ turnId: 't1', answer: 'done', durationMs: 9000, isAborted: false, usage: null })
  await clock.advance(1)
  await ui.unmount()

  const after = await $.ui.mount(band(8))
  expect(await after.find({ type: 'Text', text: 'that null check does zero work.' })).toBeDefined()
})

test('a shipped node in the fleet feed becomes a line with no model call', async ($, on) => {
  const { clock } = boot(on)
  let modelCalls = 0
  on('model.complete', () => {
    modelCalls += 1
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
  expect(await after.find({ type: 'Text', text: 'psst. parser-fix shipped pr 42.' })).toBeDefined()
  expect(modelCalls).toBe(0)
  expect(narrate({ ts: '', kind: 'session_spawned' })).toBe(null)
})
