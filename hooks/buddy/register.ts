import type { EngineInterface, On } from 'claude-code'

import { type Companion, RARITY_COLORS, RARITY_STARS, STAT_NAMES, embody, hatch, restore } from './companion'
import { IDLE_SEQUENCE, PET_HEARTS, renderFace, renderSprite } from './sprites'
import { type FeedRow, cleanReaction, narrate, quickLine, reactionPrompt, summarizeTurn, systemPrompt } from './voice'

const TICK_MS = 500
const BUBBLE_MS = 12_000
const PET_MS = 2_500
const MIN_TURN_MS = 5_000
const REACT_GAP_MS = 10_000
// ponytail: each session polls the feed on its own (about 4 s of CPU a read); share one read through $.store if fleets grow
const FEED_MS = 120_000
// A band that drew within this window has someone looking at it.
const SEEN_MS = 5_000

let buddy: Companion | null = null
let muted = false
let tick = 0
let turns = 0
let bubble: { text: string; at: number } | null = null
let pettedAt = -Infinity
let drawnAt = -Infinity
let reactedAt = -Infinity
let feedSince = 0
let feedOff = false

async function load($: EngineInterface, now: number): Promise<void> {
  muted = (await $.store.get('muted')) === true
  const saved = (await $.store.get('soul')) as Companion | undefined
  if (saved?.seed) {
    buddy = embody(saved)
    return
  }
  let soul = null
  try {
    const home = await $.env.get('HOME')
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
    '/buddy pet · /buddy roll · /buddy off',
  ].join('\n')
}

async function react($: EngineInterface, now: number): Promise<void> {
  if (!buddy) return
  const summary = summarizeTurn(await $.session.messages())
  if (!summary.trim()) return
  const reply: unknown = await $.model.complete({
    model: 'haiku',
    system: systemPrompt(buddy),
    prompt: reactionPrompt(summary),
    maxTokens: 60,
    timeoutMs: 20_000,
  })
  const r = reply as { isAnswered?: boolean; text?: string }
  const text = typeof reply === 'string' ? reply : r?.isAnswered ? (r.text ?? '') : ''
  const line = cleanReaction(text)
  if (line) say(line, now)
  $.ui.invalidate('ui.render')
}

async function readFeed($: EngineInterface, now: number): Promise<void> {
  if (feedOff || muted || !buddy || now - drawnAt > SEEN_MS) return
  let out
  try {
    out = await $.process.run(['fno-agents', 'feed', '--json', '--since-epoch', String(feedSince), '--limit', '20'], {
      timeoutMs: 15_000,
    })
  } catch {
    feedOff = true
    return
  }
  if (out.exitCode !== 0) return
  let rows: FeedRow[]
  try {
    rows = JSON.parse(out.stdout)
  } catch {
    return
  }
  let line: string | null = null
  for (const row of rows) {
    const at = Math.floor(Date.parse(row.ts) / 1000)
    if (at < feedSince) continue
    feedSince = Math.max(feedSince, at + 1)
    line = narrate(row) ?? line
  }
  if (line) {
    say(line, now)
    $.ui.invalidate('ui.render')
  }
}

export function register(on: On) {
  on('session.start', async ($, e, next) => {
    const now = await $.clock.now()
    feedSince = Math.floor(now / 1000)
    await load($, now)
    $.clock.every(TICK_MS, async () => {
      tick += 1
      if (buddy && !muted && (await $.clock.now()) - drawnAt < SEEN_MS) $.ui.invalidate('ui.render')
    })
    $.clock.every(FEED_MS, async () => readFeed($, await $.clock.now()))
    try {
      await $.command.register({ name: 'buddy', description: 'Your terminal companion: show it, pet it, roll a new one, or turn it off', argumentHint: '[pet|roll|off|on]', immediate: true })
    } catch {
      // A newer Claude Code may ship its own /buddy again; the band still draws.
    }
    return next(e)
  })

  on('command.run', { command: 'buddy' }, async ($, e) => {
    const now = await $.clock.now()
    const arg = e.args.trim().toLowerCase()
    if (!buddy) await load($, now)
    if (arg === 'off') {
      muted = true
      await $.store.set('muted', true)
      $.ui.invalidate('ui.render')
      return { text: `${buddy!.name} is napping. /buddy on wakes it.` }
    }
    if (arg === 'on') {
      muted = false
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
        react($, now).catch(() => {})
      }
    }
    return next(e)
  })

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    if (!buddy || muted) return next(e)
    const now = await $.clock.now()
    drawnAt = now
    const { Box, Text, Button } = $.ui.resolve(e)
    const color = RARITY_COLORS[buddy.rarity]
    const talking = bubble && now - bubble.at < BUBBLE_MS ? bubble.text : null
    const petting = now - pettedAt < PET_MS
    const width = e.props.bodyColumns ?? 80

    if ((e.props.maxRows ?? 0) < 6 || width < 40) {
      const face = (petting ? '♥ ' : '') + renderFace(buddy)
      return Text({ color, children: [talking ? `${face} ${buddy.name}: ${talking}` : `${face} ${buddy.name}`] })
    }

    const frame = IDLE_SEQUENCE[tick % IDLE_SEQUENCE.length]!
    const sprite = renderSprite(buddy, frame)
    if (petting) sprite[0] = PET_HEARTS[tick % PET_HEARTS.length]!
    return Box({
      flexDirection: 'row',
      columnGap: 1,
      children: [
        Box({
          flexDirection: 'column',
          children: [
            ...sprite.map(line => Text({ color, children: [line] })),
            Button({ key: 'pet', label: buddy.name, hotkey: 'p', plain: true, dimColor: true, onPress: async () => {
              pettedAt = await $.clock.now()
              $.ui.invalidate('ui.render')
            } }),
          ],
        }),
        ...(talking
          ? [Box({ borderStyle: 'round', paddingX: 1, width: Math.min(44, width - 16), children: [Text({ wrap: 'wrap', children: [talking] })] })]
          : []),
      ],
    })
  })
}
