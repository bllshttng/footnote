import type { EngineInterface, On } from 'claude-code'

import { type Companion, type Soul, RARITY_COLORS, RARITY_STARS, RARITY_THEME, STAT_NAMES, embody, hatch, restore } from './companion'
import { IDLE_SEQUENCE, PET_HEARTS, renderFace, renderSprite } from './sprites'
import { type FeedRow, cleanIdleLines, cleanPersonality, cleanReaction, idleLine, idlePrompt, narrate, personalityPrompt, reactionPrompt, summarizeTurn, systemPrompt } from './voice'

const TICK_MS = 500
const BUBBLE_MS = 30_000
// After this long with nothing said, the buddy says a canned line (no model call).
const IDLE_TALK_MS = 45_000
const PET_MS = 2_500
const MIN_TURN_MS = 5_000
const REACT_GAP_MS = 10_000
const FEED_MS = 120_000
const FEED_WINDOW_S = 600
const FLEET_MS = 300_000
// A buddy that drew within this window has someone looking at it.
const SEEN_MS = 5_000
// The status line wrapper drops a frame older than 30 s, so an idle frame is rewritten well before that.
const FRAME_REFRESH_MS = 10_000
const PANE_ID = 'buddy'
const PANE_COLUMNS = 24
const WRAPPER = 'statusline.py'
// bbb: bring back buddy.
const COMMANDS = ['buddy', 'bbb']

let buddy: Companion | null = null
let muted = false
let tick = 0
// The buddy's own idle chatter, written once per soul by the model and kept in the store.
let idleLines: string[] | undefined
let idleAsked = ''
let bubble: { text: string; at: number } | null = null
let pettedAt = -Infinity
let drawnAt = -Infinity
let paneDrawnAt = -Infinity
let paneAsked = false
let reactedAt = -Infinity
let feedSince = 0
let feedOff = false
let fleet = ''
let home = ''
// The buddy's files live in the fno state folder, or ~/.local/state/buddy without fno; never under the Claude config dir.
let stateDir = ''
let sessionId = ''
let wrapped = false
let deferred = false
let lastFrame = ''
let frameAt = -Infinity
let unwrappedAt = -Infinity

const buddyDir = () => `${stateDir}/state/buddy`
const settingsPath = () => `${home}/.claude/settings.json`
const wrapperCommand = () => `python3 ${buddyDir()}/${WRAPPER}`

async function load($: EngineInterface, now: number): Promise<void> {
  muted = (await $.store.get('muted')) === true
  const saved = (await $.store.get('soul')) as Companion | undefined
  if (saved?.seed) {
    buddy = embody(saved)
    return
  }
  let soul: Soul | null = await fromFno($)
  if (soul) {
    buddy = embody(soul)
    return
  }
  try {
    if (home) soul = restore(await $.fs.read(home + '/.claude.json'), now)
  } catch {
    soul = null
  }
  const welcome = soul ? `${soul.name} is back. did you miss me?` : null
  const fresh = !soul
  soul ??= hatch(newSeed(), now)
  await $.store.set('soul', soul)
  buddy = embody(soul)
  say(welcome ?? `hi. i'm ${buddy.name}.`, now)
  if (fresh) void givePersonality($)
}

// An fno release from before the move still loads its own copy of the buddy, which stamps fno's
// store every few minutes. While that copy runs, this one stays off so only one buddy shows.
async function oldCopyLive($: EngineInterface, now: number): Promise<boolean> {
  if (!home) return false
  const dir = `${home}/.claude/plugins/store`
  try {
    for (const entry of await $.fs.list(dir)) {
      if (!entry.name.startsWith('fno_') || !entry.name.endsWith('.json')) continue
      const old = JSON.parse(await $.fs.read(`${dir}/${entry.name}`))
      const at = Math.max(old?.fleet?.at ?? 0, old?.feed?.at ?? 0)
      if (now - at < FLEET_MS + 60_000) return true
    }
  } catch {
    // Nothing readable: no old copy to defer to.
  }
  return false
}

// The buddy used to load inside the fno plugin, whose store is a different file. Bring its soul,
// reroll bank, and mute over once, so the same buddy comes back after the move.
async function fromFno($: EngineInterface): Promise<Soul | null> {
  if (!home) return null
  const dir = `${home}/.claude/plugins/store`
  try {
    // The marketplace install (fno@footnote) wins over a local dev copy such as fno@inline.
    const entries = (await $.fs.list(dir)).sort((a, b) => Number(b.name.startsWith('fno_footnote-')) - Number(a.name.startsWith('fno_footnote-')))
    for (const entry of entries) {
      if (!entry.name.startsWith('fno_') || !entry.name.endsWith('.json')) continue
      const old = JSON.parse(await $.fs.read(`${dir}/${entry.name}`))
      if (!old?.soul?.seed) continue
      await $.store.set('soul', old.soul)
      if (old.rerolls) await $.store.set('rerolls', old.rerolls)
      if (old.muted === true) {
        muted = true
        await $.store.set('muted', true)
      }
      return old.soul as Soul
    }
  } catch {
    // No fno store, or an unreadable one: hatch as usual.
  }
  return null
}

async function writeIdleLines($: EngineInterface): Promise<void> {
  const c = buddy
  if (!c || idleAsked === c.seed) return
  idleAsked = c.seed
  const saved = (await $.store.get('idle')) as { seed: string; lines: string[] } | undefined
  if (saved?.seed === c.seed && saved.lines.length) {
    idleLines = saved.lines
    return
  }
  try {
    const reply = await $.model.complete({ model: 'haiku', system: systemPrompt(c), prompt: idlePrompt(c), maxTokens: 300, timeoutMs: 20_000 })
    const lines = reply.isAnswered ? cleanIdleLines(reply.text) : []
    if (lines.length < 4 || buddy?.seed !== c.seed) return
    idleLines = lines
    await $.store.set('idle', { seed: c.seed, lines })
  } catch {
    // The canned lines keep it talking.
  }
}

// A new buddy hatches with a placeholder; one model call then writes who it is, as the original did.
async function givePersonality($: EngineInterface): Promise<void> {
  const c = buddy
  if (!c) return
  try {
    const reply = await $.model.complete({ model: 'haiku', prompt: personalityPrompt(c, c.seed), maxTokens: 120, timeoutMs: 20_000 })
    const personality = reply.isAnswered ? cleanPersonality(reply.text) : null
    if (!personality || buddy?.seed !== c.seed) return
    const soul: Soul = { seed: c.seed, name: c.name, personality, hatchedAt: c.hatchedAt, ...(c.species ? { species: c.species } : {}) }
    await $.store.set('soul', soul)
    buddy = embody(soul)
  } catch {
    // The placeholder personality stays; the buddy still talks.
  }
}

function newSeed(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(8))
  return Array.from(bytes, b => b.toString(16).padStart(2, '0')).join('')
}

function say(text: string, now: number): void {
  bubble = { text, at: now }
}

// Rerolls are earned: one free a day, one per SHIPS_PER_REROLL PRs the fleet ships, banked up to REROLL_BANK.
// ponytail: fixed numbers; a [buddy] section in config.toml can own them once someone wants to tune them.
const REROLL_BANK = 3
const SHIPS_PER_REROLL = 2
export type Rerolls = { bank: number; day: string; ships: number; shipAt: number }

export function refill(r: Rerolls | undefined, today: string, shippedAt: number[] = []): Rerolls {
  let { bank, day, ships, shipAt } = r ?? { bank: 0, day: '', ships: 0, shipAt: 0 }
  if (day !== today) {
    bank = Math.min(REROLL_BANK, bank + 1)
    day = today
  }
  // shipAt is the newest ship already counted, so every session reading the same feed counts a PR once.
  for (const at of [...shippedAt].sort((a, b) => a - b)) {
    if (at <= shipAt) continue
    shipAt = at
    ships += 1
    if (ships >= SHIPS_PER_REROLL) {
      ships = 0
      bank = Math.min(REROLL_BANK, bank + 1)
    }
  }
  return { bank, day, ships, shipAt }
}

const today = (now: number) => new Date(now).toLocaleDateString('en-CA')

async function rerolls($: EngineInterface, now: number, shippedAt: number[] = []): Promise<Rerolls> {
  const r = refill((await $.store.get('rerolls')) as Rerolls | undefined, today(now), shippedAt)
  await $.store.set('rerolls', r)
  return r
}

function rerollLine(r: Rerolls): string {
  return `rerolls: ${r.bank}/${REROLL_BANK} · one more per ${SHIPS_PER_REROLL} shipped PRs (${r.ships}/${SHIPS_PER_REROLL}) and one each day`
}

function card(c: Companion, r: Rerolls): string {
  const stats = STAT_NAMES.map(s => `${s.padEnd(9)} ${'█'.repeat(Math.round(c.stats[s] / 10)).padEnd(10, '░')} ${c.stats[s]}`)
  return [
    `${c.name} the ${c.species}  ${RARITY_STARS[c.rarity]} ${c.rarity}${c.shiny ? ' ✨ shiny' : ''}`,
    '',
    ...renderSprite(c, 0),
    '',
    c.personality,
    '',
    ...stats,
    '',
    rerollLine(r),
    '/buddy pet · roll · statusline · pane · off · bye',
  ].join('\n')
}

async function readJson($: EngineInterface, path: string): Promise<any> {
  try {
    return JSON.parse(await $.fs.read(path))
  } catch (err) {
    if (err instanceof SyntaxError) throw err
    return undefined
  }
}

// The user's settings file is theirs: a file that does not parse is left alone.
async function readSettings($: EngineInterface): Promise<Record<string, unknown> | null> {
  try {
    return (await readJson($, settingsPath())) ?? {}
  } catch {
    return null
  }
}

function isOurs(statusLine: any): boolean {
  return typeof statusLine?.command === 'string' && statusLine.command.includes(`/state/buddy/${WRAPPER}`)
}

async function resolveStateDir($: EngineInterface): Promise<string> {
  const fallback = home ? `${home}/.local` : ''
  try {
    const out = await $.process.run(['fno', 'config', 'get', 'state_dir'], { timeoutMs: 10_000 })
    const dir = out.exitCode === 0 ? out.stdout.split('\n')[0]!.trim() : ''
    return dir ? dir.replace(/^~(?=\/|$)/, home).replace(/\/+$/, '') : fallback
  } catch {
    return fallback
  }
}

// Keeps a copy of the wrapper at a path that survives plugin updates, so statusLine never points into the plugin cache.
async function installWrapper($: EngineInterface): Promise<void> {
  const ours = await $.fs.read(`${$.plugin.root}/hooks/${WRAPPER}`)
  const target = `${buddyDir()}/${WRAPPER}`
  let theirs = ''
  try {
    theirs = await $.fs.read(target)
  } catch {
    theirs = ''
  }
  if (theirs !== ours) await $.fs.write(target, ours)
}

async function statuslineOn($: EngineInterface): Promise<string> {
  if (!stateDir) return 'The status line needs HOME to be set.'
  const settings = await readSettings($)
  if (!settings) return `${settingsPath()} does not parse, so I left it alone.`
  const current = settings.statusLine as any
  if (isOurs(current)) return `${buddy!.name} already sits beside your status line.`
  await installWrapper($)
  await $.fs.write(`${buddyDir()}/inner.json`, JSON.stringify({ statusLine: current ?? null }, null, 2) + '\n')
  settings.statusLine = { type: 'command', command: wrapperCommand(), padding: current?.padding ?? 0, refreshInterval: 1 }
  await $.fs.write(settingsPath(), JSON.stringify(settings, null, 2) + '\n')
  wrapped = true
  await $.ui.close({ id: PANE_ID })
  return current
    ? `${buddy!.name} now sits beside your status line. /buddy pane moves it to a side pane and puts yours back as it was.`
    : `${buddy!.name} now sits in a new status line. /buddy pane moves it to a side pane and removes that line.`
}

async function statuslineOff($: EngineInterface): Promise<{ ok: boolean; text: string }> {
  const settings = await readSettings($)
  if (!settings) return { ok: false, text: `${settingsPath()} does not parse, so I left it alone.` }
  const saved = stateDir ? await readJson($, `${buddyDir()}/inner.json`).catch(() => undefined) : undefined
  let text = ''
  if (isOurs(settings.statusLine)) {
    // Without the saved copy there is nothing to put back, and deleting the wrapper would leave no status line at all.
    if (!saved) return { ok: false, text: `I cannot read your saved status line in ${buddyDir()}/inner.json, so I left ${settingsPath()} alone.` }
    if (saved.statusLine) settings.statusLine = saved.statusLine
    else delete settings.statusLine
    await $.fs.write(settingsPath(), JSON.stringify(settings, null, 2) + '\n')
    text = 'Your status line is back as it was.'
  }
  // inner.json marks that the user wants the buddy beside the status line; without it no re-wrap is offered.
  if (saved) await $.fs.write(`${buddyDir()}/inner.json`, '')
  wrapped = false
  unwrappedAt = await $.clock.now()
  return { ok: true, text }
}

async function react($: EngineInterface): Promise<void> {
  if (!buddy) return
  const summary = summarizeTurn(await $.session.messages())
  if (!summary.trim()) return
  const reply = await $.model.complete({
    model: 'haiku',
    system: systemPrompt(buddy),
    prompt: reactionPrompt(summary),
    maxTokens: 80,
    timeoutMs: 20_000,
  })
  const line = reply.isAnswered ? cleanReaction(reply.text) : ''
  if (line) say(line, await $.clock.now())
  $.ui.invalidate('ui.render')
}

// One feed read serves every session on the machine: a read costs about 4 s of
// CPU, and a mux of workers would otherwise each pay it every FEED_MS.
async function feedRows($: EngineInterface, now: number): Promise<FeedRow[] | null> {
  const shared = (await $.store.get('feed')) as { at: number; rows: FeedRow[] } | undefined
  if (shared && now - shared.at < FEED_MS - 10_000) return shared.rows
  let out
  try {
    const since = Math.floor(now / 1000) - FEED_WINDOW_S
    out = await $.process.run(['fno-agents', 'feed', '--json', '--since-epoch', String(since), '--limit', '50'], {
      timeoutMs: 15_000,
    })
  } catch {
    feedOff = true
    return null
  }
  if (out.exitCode !== 0) return null
  let rows: FeedRow[]
  try {
    rows = JSON.parse(out.stdout)
  } catch {
    return null
  }
  await $.store.set('feed', { at: now, rows })
  return rows
}

async function readFeed($: EngineInterface, now: number): Promise<void> {
  if (feedOff || muted || !buddy || now - drawnAt > SEEN_MS) return
  const rows = await feedRows($, now)
  if (!rows) return
  const before = refill((await $.store.get('rerolls')) as Rerolls | undefined, today(now)).bank
  const after = await rerolls($, now, rows.filter(row => row.kind === 'node_shipped').map(row => Date.parse(row.ts)).filter(Number.isFinite))
  let line: string | null = null
  for (const row of rows) {
    const at = Math.floor(Date.parse(row.ts) / 1000)
    if (!(at >= feedSince)) continue
    feedSince = Math.max(feedSince, at + 1)
    line = narrate(row) ?? line
  }
  if (after.bank > before) line = `${line ? line + ' ' : ''}+1 reroll (${after.bank}/${REROLL_BANK}).`
  if (line) {
    say(line, now)
    $.ui.invalidate('ui.render')
  }
}

async function runJson($: EngineInterface, argv: string[], cwd?: string): Promise<any> {
  try {
    const out = await $.process.run(argv, { timeoutMs: 20_000, ...(cwd ? { cwd } : {}) })
    return out.exitCode === 0 ? JSON.parse(out.stdout) : undefined
  } catch {
    return undefined
  }
}

// The same counts the fleet's own verbs print: live workers from the spawn gate,
// questions waiting on the user from the outstanding inbox, and the user's open PRs.
// Each verb takes several seconds, so one read every FLEET_MS serves every session.
export function fleetLine(gate: any, outstanding: any, prs: any): string {
  const parts: string[] = []
  if (typeof gate?.live_workers === 'number') parts.push(`${gate.live_workers} workers`)
  if (Array.isArray(outstanding?.questions)) parts.push(`${outstanding.questions.length} asks`)
  if (Array.isArray(prs)) parts.push(`${prs.length} PRs`)
  return parts.join(' · ')
}

async function readFleet($: EngineInterface, now: number): Promise<void> {
  if (feedOff || muted || !buddy || now - drawnAt > SEEN_MS) return
  const shared = (await $.store.get('fleet')) as { at: number; line: string } | undefined
  if (shared && now - shared.at < FLEET_MS - 10_000) {
    fleet = shared.line
    return
  }
  const [gate, outstanding, prs] = await Promise.all([
    runJson($, ['fno', 'agents', 'gate-status']),
    runJson($, ['fno', 'inbox', 'outstanding', '--json']),
    runJson($, ['gh', 'pr', 'list', '--author', '@me', '--state', 'open', '--json', 'number'], await $.session.root()),
  ])
  fleet = fleetLine(gate, outstanding, prs)
  await $.store.set('fleet', { at: now, line: fleet })
}

function talking(now: number): string | null {
  return bubble && now - bubble.at < BUBBLE_MS ? bubble.text : null
}

function sprite(c: Companion, now: number): string[] {
  const lines = renderSprite(c, IDLE_SEQUENCE[tick % IDLE_SEQUENCE.length]!)
  // A 5-line sprite keeps row 0 for a hat; a shorter one has no free row, so the hearts go above it.
  if (now - pettedAt < PET_MS) lines.splice(0, lines.length < 5 ? 0 : 1, PET_HEARTS[tick % PET_HEARTS.length]!)
  return lines
}

// The wrapper stamps a heartbeat on each run, so a status line set in any settings file counts.
async function wrapperSeen($: EngineInterface, now: number): Promise<boolean> {
  if (!stateDir) return false
  try {
    const at = Number(await $.fs.read(`${buddyDir()}/frames/${sessionId}.seen`))
    // A beat from before /buddy pane is the old wrapper's last run, not a live one.
    return at > unwrappedAt && now - at < SEEN_MS
  } catch {
    return false
  }
}

// The status line wrapper reads this file; frames change on screen at each status line refresh.
async function writeFrame($: EngineInterface, now: number): Promise<void> {
  if (!buddy || !sessionId || !stateDir) return
  const frame = JSON.stringify({
    sprite: sprite(buddy, now),
    name: buddy.name,
    face: renderFace(buddy),
    color: RARITY_COLORS[buddy.rarity],
    speech: talking(now) ?? '',
    fleet,
  })
  if (frame === lastFrame && now - frameAt < FRAME_REFRESH_MS) return
  lastFrame = frame
  frameAt = now
  await $.fs.write(`${buddyDir()}/frames/${sessionId}.json`, `{"at":${now},${frame.slice(1)}`)
}

export function register(on: On) {
  on('session.start', async ($, e, next) => {
    const now = await $.clock.now()
    feedSince = Math.floor(now / 1000)
    home = (await $.env.get('HOME')) ?? ''
    sessionId = await $.session.id()
    await load($, now)
    deferred = await oldCopyLive($, now)
    if (deferred) muted = true
    // A buddy that is off runs nothing at start: no process, no settings read.
    if (!muted) {
      stateDir = await resolveStateDir($)
      const settings = await readSettings($)
      wrapped = isOurs(settings?.statusLine)
      if (wrapped && stateDir) await installWrapper($).catch(() => {})
      else if (stateDir) {
        const saved = await readJson($, `${buddyDir()}/inner.json`).catch(() => undefined)
        // The user wrapped once, then ran /statusline again: ask, never re-wrap on their behalf.
        if (saved && buddy) say(`your status line changed. /buddy statusline puts me back beside it.`, now)
      }
    }
    $.clock.every(TICK_MS, async () => {
      tick += 1
      if (!buddy || muted) return
      const at = await $.clock.now()
      if (at - drawnAt < SEEN_MS && at - (bubble?.at ?? -Infinity) > IDLE_TALK_MS) {
        say(idleLine(buddy, idleLines, bubble?.text ?? null), at)
        if (!idleLines) void writeIdleLines($)
      }
      if (tick % 4 === 0) {
        const was = wrapped
        wrapped = (await wrapperSeen($, at)) || isOurs((await readSettings($))?.statusLine)
        if (wrapped && !was) await $.ui.close({ id: PANE_ID }).catch(() => {})
        if (wrapped) $.ui.invalidate('ui.render')
      }
      if (wrapped) {
        drawnAt = at
        await writeFrame($, at).catch(() => {})
      } else if (at - drawnAt < SEEN_MS) $.ui.invalidate('ui.render')
    })
    $.clock.every(FEED_MS, async () => readFeed($, await $.clock.now()))
    $.clock.every(FLEET_MS / 5, async () => readFleet($, await $.clock.now()))
    for (const name of COMMANDS) {
      try {
        await $.command.register({ name, description: name === 'bbb' ? 'Bring back buddy: your terminal companion' : 'Your terminal companion: show it, pet it, roll a new one, or turn it off', argumentHint: '[pet|roll|statusline|pane|off|on|bye]', immediate: true })
      } catch {
        // A newer Claude Code may ship its own /buddy again; /bbb still works.
      }
    }
    return next(e)
  })

  for (const command of COMMANDS) on('command.run', { command }, async ($, e) => {
    const now = await $.clock.now()
    const arg = e.args.trim().toLowerCase()
    if (!buddy) await load($, now)
    if (deferred) return { text: 'Your fno plugin still runs its own buddy. Update fno (/plugin update fno@footnote), then start a new session.' }
    if (!stateDir) stateDir = await resolveStateDir($)
    if (arg === 'statusline') return { text: await statuslineOn($) }
    if (arg === 'bye') {
      const { ok, text } = await statuslineOff($)
      if (!ok) return { text }
      muted = true
      await $.store.set('muted', true)
      await $.ui.close({ id: PANE_ID }).catch(() => {})
      $.ui.invalidate('ui.render')
      return { text: `${text ? text + ' ' : ''}Bye from ${buddy!.name}. It is gone from every session and makes no calls. /buddy on brings it back.` }
    }
    if (arg === 'pane' || arg === 'restore') {
      const { ok, text } = await statuslineOff($)
      if (!ok) return { text }
      // Asked for by name, so the pane opens from 110 columns rather than the unasked 144.
      paneAsked = true
      await $.ui.open({ id: PANE_ID, title: buddy!.name, columns: PANE_COLUMNS }).catch(() => {})
      return { text: `${text ? text + ' ' : ''}${buddy!.name} moved to a side pane. /buddy statusline brings it back.` }
    }
    if (arg === 'off') {
      muted = true
      await $.store.set('muted', true)
      await $.ui.close({ id: PANE_ID })
      $.ui.invalidate('ui.render')
      return { text: `${buddy!.name} is napping. /buddy on wakes it.` }
    }
    if (arg === 'on') {
      muted = false
      paneAsked = false
      await $.store.set('muted', false)
    }
    if (arg === 'roll') {
      const r = await rerolls($, now)
      if (r.bank < 1) return { text: `${buddy!.name} stays. ${rerollLine(r)}.` }
      await $.store.set('rerolls', { ...r, bank: r.bank - 1 })
      const soul = hatch(newSeed(), now)
      await $.store.set('soul', soul)
      buddy = embody(soul)
      idleLines = undefined
      say(`hi. i'm ${buddy.name}.`, now)
      void givePersonality($)
    }
    if (arg === 'pet') {
      pettedAt = now
      say('♥', now)
    }
    $.ui.invalidate('ui.render')
    return { text: card(buddy!, await rerolls($, now)) }
  })

  on('turn.complete', async ($, e, next) => {
    if (buddy && !muted && !e.agentId && !e.isAborted && e.durationMs >= MIN_TURN_MS) {
      const now = await $.clock.now()
      if (now - reactedAt >= REACT_GAP_MS && now - drawnAt < SEEN_MS) {
        reactedAt = now
        // Not awaited: the next prompt must not wait on the buddy's model call.
        react($).catch(() => {})
      }
    }
    return next(e)
  })

  // The fallback when the status line is not wrapped: a narrow dock on the right,
  // the buddy standing at the bottom and its words above it.
  on('ui.render', { component: 'Pane' }, async ($, e, next) => {
    if (e.requestId !== PANE_ID || !buddy || muted) return next(e)
    // One buddy on screen: a pane left open (a resumed session, a late open) closes once the status line has it.
    if (wrapped) {
      void $.ui.close({ id: PANE_ID }).catch(() => {})
      return next(e)
    }
    const now = await $.clock.now()
    drawnAt = paneDrawnAt = now
    const { Box, Text, Button } = $.ui.resolve(e)
    const color = RARITY_THEME[buddy.rarity]
    const words = talking(now)
    return Box({
      flexDirection: 'column',
      justifyContent: 'flex-end',
      height: e.props.scroll?.bodyRows ?? 12,
      children: [
        ...(words ? [Text({ wrap: 'wrap', children: [words] }), Text({ children: [' '] })] : []),
        ...(fleet ? [Text({ dimColor: true, wrap: 'wrap', children: [fleet] }), Text({ children: [' '] })] : []),
        ...sprite(buddy, now).map(line => Text({ color, children: [line] })),
        Button({ key: 'pet', label: buddy.name, hotkey: 'p', plain: true, dimColor: true, onPress: async () => {
          pettedAt = await $.clock.now()
          $.ui.invalidate('ui.render')
        } }),
      ],
    })
  })

  // The band only holds a one-line face, and only where neither the status line nor the dock has the buddy.
  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    if (!buddy || muted || wrapped || e.props.hasSurvey) return next(e)
    const now = await $.clock.now()
    if (now - paneDrawnAt < SEEN_MS) return next(e)
    if (!paneAsked && e.viewport?.isFullscreen === true) {
      paneAsked = true
      void $.ui.open({ id: PANE_ID, title: buddy.name, columns: PANE_COLUMNS }).catch(() => {})
    }
    drawnAt = now
    const { Box, Text } = $.ui.resolve(e)
    const words = talking(now)
    const face = (now - pettedAt < PET_MS ? '♥ ' : '') + renderFace(buddy)
    const ours = Text({ children: [Text({ color: RARITY_THEME[buddy.rarity], children: [`${face} ${buddy.name}`] }), ...(words ? [`: ${words}`] : [])] })
    const theirs = await next(e)
    return theirs ? Box({ flexDirection: 'column', children: [ours, theirs] }) : ours
  })
}
