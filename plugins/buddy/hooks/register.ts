import type { EngineInterface, On } from 'claude-code'

import { type Companion, type Soul, RARITY_COLORS, RARITY_STARS, RARITY_THEME, STAT_NAMES, type StatName, embody, hatch, restore } from './companion'
import { HATCH_FRAMES, HATCH_FRAME_MS, HATCH_MIN_ROUNDS, HATCH_WOBBLE, IDLE_SEQUENCE, PET_HEARTS, RAINBOW, renderFace, renderSprite } from './sprites'
import { type FeedRow, addressedBy, cleanPersonality, cleanReaction, idlePrompt, lastPrompt, loudReason, type Reason, turnOutput, newsFact, newsPrompt, personalityPrompt, reactionPrompt, summarizeTurn, systemPrompt } from './voice'

const TICK_MS = 500
const BUBBLE_MS = 30_000
// After this long with nothing said, the buddy says a canned line (no model call).
// Quiet this long, the buddy says something of its own: one model call, like a reaction.
const IDLE_TALK_MS = 120_000
const PET_MS = 2_500
// The original's gap between ordinary turn reactions.
const REACT_GAP_MS = 30_000
const FEED_MS = 120_000
const FEED_WINDOW_S = 600
const FLEET_MS = 300_000
// A buddy that drew within this window has someone looking at it.
const SEEN_MS = 5_000
// The status line runs in every live session, hidden mux panes too, so a draw does not mean a
// person is there. A key typed in a session's prompt box does: for this long after one, the
// session reacts to its turns, and the session typed in last speaks for the machine.
const TYPED_MS = 600_000
const SPEAKER_STAMP_MS = 30_000
// The status line wrapper drops a frame older than 30 s, so an idle frame is rewritten well before that.
const FRAME_REFRESH_MS = 10_000
const PANE_ID = 'buddy'
// The /buddy card opens here, focused, so any key closes it like the original.
const CARD_ID = 'buddy-card'
let cardSnap: Shown | undefined
const PANE_COLUMNS = 24
const WRAPPER = 'statusline.py'
// bbb: bring back buddy.
const COMMANDS = ['buddy', 'bbb']

let buddy: Companion | null = null
let muted = false
let tick = 0
let bubble: { text: string; at: number } | null = null
let pettedAt = -Infinity
let drawnAt = -Infinity
let typedAt = -Infinity
let stampedAt = -Infinity
let paneDrawnAt = -Infinity
let paneAsked = false
let reactedAt = -Infinity
let recent: string[] = []
// The /buddy output row draws as the card; a fresh soul's first card plays the hatch first.
const CARD_MARK = '\u2063'
const HATCH_MARK = '\u2064'
type Shown = { c: Companion; last: string; r: Rerolls; hatchAt?: number; crackAt?: number }
const shown = new Map<string, Shown>()
let pending: Shown | undefined
let hatchUntil = -Infinity
// False while a fresh soul waits for its model-written personality; the hatch holds on the wobble until then.
let personalityDone = true
let lastSaid = ''
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
  if (fresh) {
    personalityDone = false
    void givePersonality($)
  }
}

// The soul is shared by every session on the machine. A roll or a new personality in one
// session rewrites it; the others take it here, so all sessions show the same buddy.
async function syncSoul($: EngineInterface): Promise<void> {
  const saved = (await $.store.get('soul')) as Soul | undefined
  if (!buddy || !saved?.seed) return
  if (saved.seed === buddy.seed && saved.name === buddy.name && saved.personality === buddy.personality) return
  if (saved.seed !== buddy.seed) {
    recent = []
    lastSaid = ''
    // Drop the old buddy's line but keep its time, so the swap does not start an idle call.
    if (bubble) bubble = { text: '', at: bubble.at }
  }
  buddy = embody(saved)
}

// Idle talk and fleet news cost a model call each. Only the session typed in last makes them,
// and only while someone typed there lately, so hidden sessions and an empty desk stay quiet.
async function speaking($: EngineInterface, now: number): Promise<boolean> {
  if (now - typedAt >= TYPED_MS) return false
  return (await $.store.get('speaker')) === sessionId
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

// A new buddy hatches with a placeholder; one model call then writes who it is, as the original did.
async function givePersonality($: EngineInterface): Promise<void> {
  const c = buddy
  if (!c) return
  try {
    const reply = await $.model.complete({ model: 'haiku', prompt: personalityPrompt(c, c.seed), maxTokens: 200, timeoutMs: 20_000 })
    const personality = reply.isAnswered ? cleanPersonality(reply.text) : null
    if (!personality || buddy?.seed !== c.seed) return
    const soul: Soul = { seed: c.seed, name: c.name, personality, hatchedAt: c.hatchedAt, ...(c.species ? { species: c.species } : {}) }
    await $.store.set('soul', soul)
    buddy = embody(soul)
  } catch {
    // The placeholder personality stays; the buddy still talks.
  } finally {
    if (buddy?.seed === c.seed) personalityDone = true
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

// The original card: rarity and species on top, the sprite, the name, the quoted personality,
// the stat bars, and the last thing it said.
function cardTree(ui: any, c: Companion, said: string, r: Rerolls): any {
  const { Box, Text } = ui
  const color = RARITY_THEME[c.rarity]
  const stat = (s: StatName) => {
    const v = c.stats[s]
    const n = Math.round(v / 10)
    return Box({ flexDirection: 'row', children: [Box({ width: 11, children: [Text({ children: [s] })] }), Text({ children: ['█'.repeat(n) + '░'.repeat(10 - n) + ' '] }), Text({ dimColor: true, children: [String(v).padStart(3)] })] })
  }
  return Box({
    flexDirection: 'column',
    borderStyle: 'round',
    borderColor: color,
    paddingX: 2,
    paddingY: 1,
    width: 40,
    flexShrink: 0,
    children: [
      Box({ justifyContent: 'space-between', children: [Text({ bold: true, color, children: [`${RARITY_STARS[c.rarity]} ${c.rarity.toUpperCase()}`] }), Text({ color, children: [c.species.toUpperCase()] })] }),
      ...(c.shiny ? [Text({ color: 'warning', bold: true, children: ['✨ SHINY ✨'] })] : []),
      Box({ flexDirection: 'column', marginY: 1, children: drawArt(ui, c, renderSprite(c, 0)) }),
      Text({ bold: true, children: [c.name] }),
      Box({ marginY: 1, children: [Text({ dimColor: true, italic: true, children: [`"${c.personality}"`] })] }),
      Box({ flexDirection: 'column', children: STAT_NAMES.map(stat) }),
      ...(said
        ? [Box({ flexDirection: 'column', marginTop: 1, children: [Text({ dimColor: true, children: ['last said'] }), Box({ borderStyle: 'round', borderColor: 'inactive', paddingX: 1, children: [Text({ dimColor: true, italic: true, children: [said] })] })] })]
        : []),
      Box({ marginTop: 1, children: [Text({ dimColor: true, children: [`rerolls ${r.bank}/${REROLL_BANK}`] })] }),
    ],
  })
}

// The hatch while it plays, then the card; in the focused pane, with the original's footer and a key to close.
function showTree(ui: any, snap: Shown, now: number, close: any): any {
  if (snap.hatchAt === undefined) return close ? ui.Box({ flexDirection: 'column', children: [cardTree(ui, snap.c, snap.last, snap.r), ui.Box({ marginTop: 1, children: [close] })] }) : cardTree(ui, snap.c, snap.last, snap.r)
  const tick = Math.floor((now - snap.hatchAt) / HATCH_FRAME_MS)
  // The soul is ready once the model wrote its personality, or after 8 s without one.
  const ready = (buddy?.seed === snap.c.seed && personalityDone) || now - snap.hatchAt > 8_000
  if (snap.crackAt === undefined && ready && tick >= HATCH_MIN_ROUNDS * HATCH_WOBBLE) snap.crackAt = tick
  const frame = snap.crackAt === undefined ? tick % HATCH_WOBBLE : Math.min(HATCH_WOBBLE + tick - snap.crackAt, HATCH_FRAMES.length)
  if (frame >= HATCH_FRAMES.length) {
    const c = buddy?.seed === snap.c.seed ? buddy : snap.c
    const said = lastSaid || snap.last
    const { Box, Text } = ui
    return Box({
      flexDirection: 'column',
      children: [
        cardTree(ui, c, said, snap.r),
        Box({
          flexDirection: 'column',
          marginTop: 1,
          children: [
            Text({ dimColor: true, children: [`${c.name} is here · it'll chime in as you code`] }),
            Text({ dimColor: true, children: ['each line is one small model call on your plan'] }),
            Text({ dimColor: true, children: ['say its name to get its take · /buddy pet · /buddy off'] }),
            ...(close ? [Box({ marginTop: 1, children: [close] })] : []),
          ],
        }),
      ],
    })
  }
  const f = HATCH_FRAMES[frame]!
  const { Box, Text } = ui
  return Box({
    flexDirection: 'column',
    alignItems: 'center',
    borderStyle: 'round',
    borderColor: RAINBOW[tick % RAINBOW.length],
    paddingY: 1,
    children: [
      ...f.lines.map(l => Text({ children: [' '.repeat(1 + f.offset) + l + ' '.repeat(1 - f.offset)] })),
      Box({
        flexDirection: 'column',
        alignItems: 'center',
        marginTop: 1,
        children: [
          Text({ dimColor: true, children: ['hatching a coding buddy…'] }),
          Text({ dimColor: true, children: ["it'll watch you work and occasionally have opinions"] }),
        ],
      }),
    ],
  })
}

// What a fresh buddy sees first, as the original read it: the package name and the last commits.
async function projectContext($: EngineInterface): Promise<string> {
  const root = await $.session.root().catch(() => '')
  const parts: string[] = []
  try {
    const pkg = JSON.parse(await $.fs.read(`${root}/package.json`))
    if (pkg.name) parts.push(`project: ${pkg.name}${pkg.description ? ' - ' + pkg.description : ''}`)
  } catch {
    // No package.json.
  }
  const log = await $.process.run(['git', '-C', root || '.', 'log', '--oneline', '-n', '3'], { timeoutMs: 5_000 }).catch(() => null)
  if (log?.exitCode === 0 && log.stdout.trim()) parts.push(`recent commits:\n${log.stdout.trim()}`)
  return parts.join('\n') || '(fresh project, nothing to see yet)'
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

// The original observer: one model call per reaction. Loud turns, a mention of the name, a pet,
// and a hatch skip the quiet gap; an ordinary turn waits it out. The last three lines ride along
// so the buddy does not repeat itself.
async function react($: EngineInterface, why: Reason | 'idle' = 'turn', context?: string): Promise<void> {
  const c = buddy
  if (!c) return
  const messages = await $.session.messages()
  const summary = context ?? summarizeTurn(messages)
  if (why === 'turn' && !summary.trim()) return
  const reply = await $.model.complete({
    model: 'haiku',
    system: systemPrompt(c),
    prompt: why === 'idle' ? idlePrompt(summary) : reactionPrompt(summary, why, recent),
    maxTokens: 160,
    timeoutMs: 20_000,
  })
  const line = reply.isAnswered ? cleanReaction(reply.text) : ''
  if (!line || buddy?.seed !== c.seed) return
  recent = [...recent, line].slice(-3)
  lastSaid = line
  const now = await $.clock.now()
  say(line, now)
  $.ui.invalidate('ui.render')
  await remember($, c, why, line, now)
}

// Every observation the buddy makes, one JSON row each, so you can read them back. The mods API
// has no append, so each write rewrites the file; the cap keeps that cheap.
// ponytail: two sessions writing at once can drop a row; an append call would fix it if the API gains one.
const OBSERVATIONS_KEPT = 1000
async function remember($: EngineInterface, c: Companion, why: string, line: string, now: number): Promise<void> {
  const path = `${buddyDir()}/observations.jsonl`
  let old: string[] = []
  try {
    old = (await $.fs.read(path)).split('\n').filter(Boolean)
  } catch {}
  // Two writes that cross can leave a torn row; drop it here so it does not stay in the file.
  old = old.filter(row => {
    try {
      return JSON.parse(row) && true
    } catch {
      return false
    }
  })
  const rows = [...old, JSON.stringify({ at: new Date(now).toISOString(), name: c.name, why, line })].slice(-OBSERVATIONS_KEPT)
  await $.fs.write(path, rows.join('\n') + '\n').catch(() => {})
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
    line = newsFact(row) ?? line
  }
  // Every session counts ships, so none is missed; only the speaker tells them.
  if (!(await speaking($, now))) return
  const earned = after.bank > before ? `+1 reroll (${after.bank}/${REROLL_BANK})` : ''
  if (line && buddy) {
    const c = buddy
    const reply = await $.model.complete({ model: 'haiku', system: systemPrompt(c), prompt: newsPrompt(line), maxTokens: 160, timeoutMs: 20_000 }).catch(() => null)
    // Unvoiced, the fact still gets through: a question waiting on the user must not vanish.
    line = (reply?.isAnswered && cleanReaction(reply.text)) || line
  }
  line = [line, earned].filter(Boolean).join(' ')
  if (line) {
    say(line, now)
    $.ui.invalidate('ui.render')
    if (buddy) await remember($, buddy, 'news', line, now)
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
  return bubble?.text && now - bubble.at < BUBBLE_MS ? bubble.text : null
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

const BUBBLE_COLUMNS = 34

// Desktop sets text in a proportional font, which collapses the spaces in a sprite. There the
// sprite is an SVG in a monospace font; SVG cannot read theme keys, so it takes a fixed color.
let desktop = false
const SVG_COLORS: Record<string, string> = { common: '#8a8a8a', uncommon: '#4caf50', rare: '#3fa7d6', epic: '#b36ae2', legendary: '#e0a526' }
const esc = (t: string) => t.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
function drawArt(ui: any, c: Companion, lines: string[]): any[] {
  if (!desktop) return lines.map(line => ui.Text({ color: RARITY_THEME[c.rarity], children: [line] }))
  const w = Math.ceil(Math.max(...lines.map(l => l.length)) * 8.4) + 2
  const h = lines.length * 17
  const rows = lines.map((l, i) => `<text x="0" y="${i * 17 + 13}" xml:space="preserve">${esc(l)}</text>`).join('')
  return [ui.Svg({ alt: `${c.name} the ${c.species}`, width: w, height: h, source: `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${h}" font-family="ui-monospace,Menlo,monospace" font-size="14" fill="${SVG_COLORS[c.rarity]}">${rows}</svg>` })]
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
      if (tick % 4 === 0) {
        if (at - drawnAt < SEEN_MS && at - (bubble?.at ?? -Infinity) > IDLE_TALK_MS && (await speaking($, at))) {
          // Stamp first so a slow call is not asked twice; the line shows when it arrives.
          bubble = { text: '', at }
          react($, 'idle').catch(() => {})
        }
        await syncSoul($).catch(() => {})
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
    $.clock.every(HATCH_FRAME_MS, async () => {
      if ((await $.clock.now()) < hatchUntil) $.ui.invalidate('ui.render')
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
      let soul = hatch(newSeed(), now)
      for (let i = 0; i < 20 && soul.name === buddy!.name; i++) soul = hatch(newSeed(), now)
      await $.store.set('soul', soul)
      buddy = embody(soul)
      recent = []
      lastSaid = ''
      personalityDone = false
      void givePersonality($)
    }
    if (arg === 'pet') {
      pettedAt = now
      reactedAt = now
      react($, 'pet', '(you were just petted)').catch(() => {})
    }
    const r = await rerolls($, now)
    const fresh = (await $.store.get('hatchSeen')) !== buddy!.seed
    if (fresh) {
      await $.store.set('hatchSeen', buddy!.seed)
      hatchUntil = now + 20_000
      // Like the original, the hello comes once the soul is written, so it speaks as itself.
      void (async () => {
        for (let i = 0; i < 16 && !personalityDone; i++) await $.clock.sleep(500)
        await react($, 'hatch', await projectContext($))
      })().catch(() => {})
    }
    const snap: Shown = { c: buddy!, last: lastSaid, r, ...(fresh ? { hatchAt: now } : {}) }
    try {
      cardSnap = snap
      await $.ui.open({ id: CARD_ID, title: buddy!.name, focus: true, closeOnEscape: true })
      $.ui.invalidate('ui.render')
      return { text: `${buddy!.name} the ${buddy!.species} · ${RARITY_STARS[buddy!.rarity]} ${buddy!.rarity}` }
    } catch {
      // No pane here (a narrow terminal, another app): the card draws in the output row instead.
      cardSnap = undefined
      pending = snap
      $.ui.invalidate('ui.render')
      return { text: (fresh ? HATCH_MARK : CARD_MARK) + card(buddy!, r) }
    }
  })

  on('prompt.edit', async ($, e, next) => {
    typedAt = await $.clock.now()
    if (typedAt - stampedAt >= SPEAKER_STAMP_MS) {
      stampedAt = typedAt
      await $.store.set('speaker', sessionId).catch(() => {})
    }
    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    if (buddy && !muted && !e.agentId && !e.isAborted) {
      const now = await $.clock.now()
      if (now - drawnAt < SEEN_MS && now - typedAt < TYPED_MS) {
        const messages = await $.session.messages()
        const why: Reason = addressedBy(lastPrompt(messages), buddy.name) ? 'addressed' : loudReason(turnOutput(messages)) ?? 'turn'
        if (why !== 'turn' || now - reactedAt >= REACT_GAP_MS) {
          reactedAt = now
          // Not awaited: the next prompt must not wait on the buddy's model call.
          react($, why).catch(() => {})
        }
      }
    }
    return next(e)
  })

  // The /buddy row: the card the original drew, or the hatch that leads into it.
  on('ui.render', { component: 'CommandOutput' }, async ($, e, next) => {
    desktop = e.surface === 'desktop'
    const text = String(e.props.text ?? '')
    if (!text.startsWith(CARD_MARK) && !text.startsWith(HATCH_MARK)) return next(e)
    let snap = shown.get(e.requestId)
    if (!snap) {
      if (!pending) return next(e)
      snap = pending
      pending = undefined
      shown.set(e.requestId, snap)
    }
    return showTree($.ui.resolve(e), snap, await $.clock.now(), false)
  })

  on('ui.render', { component: 'Pane' }, async ($, e, next) => {
    desktop = e.surface === 'desktop'
    if (e.requestId !== CARD_ID || !cardSnap) return next(e)
    const ui = $.ui.resolve(e)
    const shut = () => {
      cardSnap = undefined
      void $.ui.close({ id: CARD_ID }).catch(() => {})
    }
    // An empty field holds the focus: any typed key or Enter closes the card, and Esc does too.
    // Desktop draws an Input as a text box, so there the card closes with a button or Esc.
    const close = e.surface === 'desktop'
      ? ui.Button({ key: 'close', label: 'close', onPress: shut })
      : ui.Input({ key: 'close', placeholder: 'press any key', value: '', submitLabel: 'close', autoFocus: true, onInput: shut, onSubmit: shut })
    return showTree(ui, cardSnap, await $.clock.now(), close)
  })

  // The fallback when the status line is not wrapped: a narrow dock on the right,
  // the buddy standing at the bottom and its words above it.
  on('ui.render', { component: 'Pane' }, async ($, e, next) => {
    desktop = e.surface === 'desktop'
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
        ...(words ? [Box({ borderStyle: 'round', children: [Text({ wrap: 'wrap', children: [words] })] }), Text({ children: ['  ◦ ·'] })] : []),
        ...(fleet ? [Text({ dimColor: true, wrap: 'wrap', children: [fleet] }), Text({ children: [' '] })] : []),
        ...drawArt({ Text, Svg: $.ui.resolve(e).Svg }, buddy, sprite(buddy, now)),
        Button({ key: 'pet', label: buddy.name, hotkey: 'p', plain: true, dimColor: true, onPress: async () => {
          pettedAt = await $.clock.now()
          $.ui.invalidate('ui.render')
        } }),
      ],
    })
  })

  // The band only holds a one-line face, and only where neither the status line nor the dock has the buddy.
  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    desktop = e.surface === 'desktop'
    // Desktop draws no status line but shares its settings, so a wrapped status line hides nothing there.
    if (!buddy || muted || (wrapped && e.surface !== 'desktop') || e.props.hasSurvey) return next(e)
    const now = await $.clock.now()
    if (now - paneDrawnAt < SEEN_MS) return next(e)
    if (!paneAsked && e.viewport?.isFullscreen === true) {
      paneAsked = true
      void $.ui.open({ id: PANE_ID, title: buddy.name, columns: PANE_COLUMNS }).catch(() => {})
    }
    drawnAt = now
    const { Box, Text } = $.ui.resolve(e)
    const words = talking(now)
    const art = sprite(buddy, now)
    // Desktop has room above the input: the full buddy stands at the right edge, its bubble to its left.
    if (e.surface === 'desktop' && (e.props.maxRows ?? 0) > art.length) {
      const color = RARITY_THEME[buddy.rarity]
      const ours = Box({
        flexDirection: 'row',
        justifyContent: 'flex-end',
        alignItems: 'flex-end',
        children: [
          ...(words ? [Box({ borderStyle: 'round', width: BUBBLE_COLUMNS, children: [Text({ wrap: 'wrap', children: [words] })] }), Text({ children: [' ◦ · '] })] : []),
          Box({ flexDirection: 'column', alignItems: 'center', children: [...drawArt({ Text, Svg: $.ui.resolve(e).Svg }, buddy, art), Text({ bold: true, children: [buddy.name] })] }),
        ],
      })
      const theirs = await next(e)
      return theirs ? Box({ flexDirection: 'column', children: [ours, theirs] }) : ours
    }
    const face = (now - pettedAt < PET_MS ? '♥ ' : '') + renderFace(buddy)
    const ours = Text({ children: [Text({ color: RARITY_THEME[buddy.rarity], children: [`${face} ${buddy.name}`] }), ...(words ? [`: ${words}`] : [])] })
    const theirs = await next(e)
    return theirs ? Box({ flexDirection: 'column', children: [ours, theirs] }) : ours
  })
}
