import type { EngineInterface, On } from 'claude-code'

import { type Companion, RARITY_COLORS, RARITY_STARS, STAT_NAMES, embody, hatch, restore } from './companion'
import { IDLE_SEQUENCE, PET_HEARTS, renderFace, renderSprite } from './sprites'
import { type FeedRow, cleanReaction, narrate, quickLine, reactionPrompt, summarizeTurn, systemPrompt } from './voice'

const TICK_MS = 500
const BUBBLE_MS = 12_000
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

let buddy: Companion | null = null
let muted = false
let tick = 0
let turns = 0
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
let sessionId = ''
let wrapped = false
let lastFrame = ''
let frameAt = -Infinity

const buddyDir = () => `${home}/.claude/buddy`
const settingsPath = () => `${home}/.claude/settings.json`
const wrapperCommand = () => `python3 ${buddyDir()}/${WRAPPER}`

async function load($: EngineInterface, now: number): Promise<void> {
  muted = (await $.store.get('muted')) === true
  const saved = (await $.store.get('soul')) as Companion | undefined
  if (saved?.seed) {
    buddy = embody(saved)
    return
  }
  let soul = null
  try {
    if (home) soul = restore(await $.fs.read(home + '/.claude.json'), now)
  } catch {
    soul = null
  }
  const welcome = soul ? `${soul.name} is back. did you miss me?` : null
  soul ??= hatch(newSeed(), now)
  await $.store.set('soul', soul)
  buddy = embody(soul)
  say(welcome ?? `hi. i'm ${buddy.name}.`, now)
}

function newSeed(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(8))
  return Array.from(bytes, b => b.toString(16).padStart(2, '0')).join('')
}

function say(text: string, now: number): void {
  bubble = { text, at: now }
}

function card(c: Companion): string {
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
    '/buddy pet · /buddy roll · /buddy off · /buddy statusline [off]',
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
  return typeof statusLine?.command === 'string' && statusLine.command.includes(`/.claude/buddy/${WRAPPER}`)
}

// Keeps a copy of the wrapper at a path that survives plugin updates, so statusLine never points into the plugin cache.
async function installWrapper($: EngineInterface): Promise<void> {
  const ours = await $.fs.read(`${$.plugin.root}/hooks/buddy/${WRAPPER}`)
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
    ? `${buddy!.name} now sits beside your status line. /buddy statusline off puts yours back as it was.`
    : `${buddy!.name} now sits in a new status line. /buddy statusline off removes it.`
}

async function statuslineOff($: EngineInterface): Promise<string> {
  const settings = await readSettings($)
  if (!settings) return `${settingsPath()} does not parse, so I left it alone.`
  const saved = await readJson($, `${buddyDir()}/inner.json`).catch(() => undefined)
  if (isOurs(settings.statusLine)) {
    if (saved?.statusLine) settings.statusLine = saved.statusLine
    else delete settings.statusLine
    await $.fs.write(settingsPath(), JSON.stringify(settings, null, 2) + '\n')
  }
  // inner.json marks that the user wants the buddy beside the status line; without it no re-wrap is offered.
  if (saved) await $.fs.write(`${buddyDir()}/inner.json`, '')
  wrapped = false
  return 'Your status line is back as it was.'
}

async function react($: EngineInterface): Promise<void> {
  if (!buddy) return
  const summary = summarizeTurn(await $.session.messages())
  if (!summary.trim()) return
  const reply = await $.model.complete({
    model: 'haiku',
    system: systemPrompt(buddy),
    prompt: reactionPrompt(summary),
    maxTokens: 60,
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
  let line: string | null = null
  for (const row of rows) {
    const at = Math.floor(Date.parse(row.ts) / 1000)
    if (!(at >= feedSince)) continue
    feedSince = Math.max(feedSince, at + 1)
    line = narrate(row) ?? line
  }
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
  try {
    return now - Number(await $.fs.read(`${buddyDir()}/frames/${sessionId}.seen`)) < SEEN_MS
  } catch {
    return false
  }
}

// The status line wrapper reads this file; frames change on screen at each status line refresh.
async function writeFrame($: EngineInterface, now: number): Promise<void> {
  if (!buddy || !sessionId) return
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
    const settings = await readSettings($)
    wrapped = isOurs(settings?.statusLine)
    if (wrapped) await installWrapper($).catch(() => {})
    else {
      const saved = await readJson($, `${buddyDir()}/inner.json`).catch(() => undefined)
      // The user wrapped once, then ran /statusline again: ask, never re-wrap on their behalf.
      if (saved && buddy) say(`your status line changed. /buddy statusline puts me back beside it.`, now)
    }
    $.clock.every(TICK_MS, async () => {
      tick += 1
      if (!buddy || muted) return
      const at = await $.clock.now()
      if (tick % 4 === 0) {
        const was = wrapped
        wrapped = (await wrapperSeen($, at)) || isOurs((await readSettings($))?.statusLine)
        if (wrapped && !was) await $.ui.close({ id: PANE_ID }).catch(() => {})
      }
      if (wrapped) {
        drawnAt = at
        await writeFrame($, at).catch(() => {})
      } else if (at - drawnAt < SEEN_MS) $.ui.invalidate('ui.render')
    })
    $.clock.every(FEED_MS, async () => readFeed($, await $.clock.now()))
    $.clock.every(FLEET_MS / 5, async () => readFleet($, await $.clock.now()))
    try {
      await $.command.register({ name: 'buddy', description: 'Your terminal companion: show it, pet it, roll a new one, or turn it off', argumentHint: '[pet|roll|off|on|statusline [off]]', immediate: true })
    } catch {
      // A newer Claude Code may ship its own /buddy again; the buddy still draws.
    }
    return next(e)
  })

  on('command.run', { command: 'buddy' }, async ($, e) => {
    const now = await $.clock.now()
    const arg = e.args.trim().toLowerCase()
    if (!buddy) await load($, now)
    if (arg === 'statusline') return { text: await statuslineOn($) }
    if (arg === 'statusline off') return { text: await statuslineOff($) }
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
      const soul = hatch(newSeed(), now)
      await $.store.set('soul', soul)
      buddy = embody(soul)
      say(`hi. i'm ${buddy.name}.`, now)
    }
    if (arg === 'pet') {
      pettedAt = now
      say('♥', now)
    }
    $.ui.invalidate('ui.render')
    return { text: card(buddy!) }
  })

  on('turn.complete', async ($, e, next) => {
    if (buddy && !muted && !e.agentId && !e.isAborted && e.durationMs >= MIN_TURN_MS) {
      const now = await $.clock.now()
      turns += 1
      say(quickLine(buddy, turns), now)
      $.ui.invalidate('ui.render')
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
    const now = await $.clock.now()
    drawnAt = paneDrawnAt = now
    const { Box, Text, Button } = $.ui.resolve(e)
    const color = RARITY_COLORS[buddy.rarity]
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
    const ours = Text({ color: RARITY_COLORS[buddy.rarity], children: [words ? `${face} ${buddy.name}: ${words}` : `${face} ${buddy.name}`] })
    const theirs = await next(e)
    return theirs ? Box({ flexDirection: 'column', children: [ours, theirs] }) : ours
  })
}
