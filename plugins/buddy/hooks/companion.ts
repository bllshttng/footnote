export const RARITIES = ['common', 'uncommon', 'rare', 'epic', 'legendary'] as const
export type Rarity = (typeof RARITIES)[number]

export const SPECIES = [
  'duck', 'goose', 'blob', 'cat', 'dragon', 'octopus', 'owl',
  'penguin', 'turtle', 'snail', 'ghost', 'axolotl', 'capybara',
  'cactus', 'robot', 'rabbit', 'mushroom', 'chonk',
] as const
export type Species = (typeof SPECIES)[number]

export const EYES = ['·', '✦', '×', '◉', '@', '°'] as const
export const HATS = ['none', 'role', 'tophat', 'propeller', 'halo', 'wizard', 'beanie', 'tinyduck'] as const
export type Hat = (typeof HATS)[number]

export const STAT_NAMES = ['DEBUGGING', 'PATIENCE', 'CHAOS', 'WISDOM', 'SNARK'] as const
export type StatName = (typeof STAT_NAMES)[number]

export type Bones = {
  rarity: Rarity
  species: Species
  eye: string
  hat: Hat
  shiny: boolean
  stats: Record<StatName, number>
}

// What the store keeps. Bones are derived from the seed on every load.
export type Soul = {
  seed: string
  name: string
  personality: string
  hatchedAt: number
  species?: Species
}

export type Companion = Bones & Soul

const RARITY_WEIGHTS: Record<Rarity, number> = { common: 60, uncommon: 25, rare: 10, epic: 4, legendary: 1 }
const RARITY_FLOOR: Record<Rarity, number> = { common: 5, uncommon: 15, rare: 25, epic: 35, legendary: 50 }
export const RARITY_STARS: Record<Rarity, string> = {
  common: '★', uncommon: '★★', rare: '★★★', epic: '★★★★', legendary: '★★★★★',
}
// The original's theme keys: Claude Code draws them in the user's theme.
export const RARITY_THEME: Record<Rarity, string> = {
  common: 'inactive', uncommon: 'success', rare: 'permission', epic: 'autoAccept', legendary: 'warning',
}

// The status line and a Desktop SVG cannot name a theme key, so they take the value Claude Code
// gives that key in each built-in theme (copied from Claude Code 2.1.294). A custom theme draws as dark.
type ThemeKey = (typeof RARITY_THEME)[Rarity]
const THEMES: Record<string, Record<ThemeKey, string>> = {
  dark: { inactive: 'rgb(153,153,153)', success: 'rgb(78,186,101)', permission: 'rgb(177,185,249)', autoAccept: 'rgb(175,135,255)', warning: 'rgb(255,193,7)' },
  light: { inactive: 'rgb(102,102,102)', success: 'rgb(44,122,57)', permission: 'rgb(87,105,247)', autoAccept: 'rgb(135,0,255)', warning: 'rgb(150,108,30)' },
  'dark-daltonized': { inactive: 'rgb(153,153,153)', success: 'rgb(51,153,255)', permission: 'rgb(153,204,255)', autoAccept: 'rgb(175,135,255)', warning: 'rgb(255,204,0)' },
  'light-daltonized': { inactive: 'rgb(102,102,102)', success: 'rgb(0,102,153)', permission: 'rgb(51,102,255)', autoAccept: 'rgb(135,0,255)', warning: 'rgb(255,153,0)' },
  'dark-ansi': { inactive: 'ansi:white', success: 'ansi:greenBright', permission: 'ansi:blueBright', autoAccept: 'ansi:magentaBright', warning: 'ansi:yellowBright' },
  'light-ansi': { inactive: 'ansi:blackBright', success: 'ansi:green', permission: 'ansi:blue', autoAccept: 'ansi:magenta', warning: 'ansi:yellow' },
}

export function rarityColor(theme: string, rarity: Rarity): string {
  return (THEMES[theme] ?? THEMES.dark!)[RARITY_THEME[rarity]]!
}

function mulberry32(seed: number): () => number {
  let a = seed >>> 0
  return () => {
    a = (a + 0x6d2b79f5) | 0
    let t = Math.imul(a ^ (a >>> 15), 1 | a)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

// FNV-1a. The old in-binary roll used Bun.hash, which a mod cannot count on.
function hash(s: string): number {
  let h = 2166136261
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i)
    h = Math.imul(h, 16777619)
  }
  return h >>> 0
}

function pick<T>(rng: () => number, arr: readonly T[]): T {
  return arr[Math.floor(rng() * arr.length)]!
}

function rollRarity(rng: () => number): Rarity {
  let roll = rng() * 100
  for (const rarity of RARITIES) {
    roll -= RARITY_WEIGHTS[rarity]
    if (roll < 0) return rarity
  }
  return 'common'
}

function rollStats(rng: () => number, rarity: Rarity): Record<StatName, number> {
  const floor = RARITY_FLOOR[rarity]
  const peak = pick(rng, STAT_NAMES)
  let dump = pick(rng, STAT_NAMES)
  while (dump === peak) dump = pick(rng, STAT_NAMES)
  const stats = {} as Record<StatName, number>
  for (const name of STAT_NAMES) {
    if (name === peak) stats[name] = Math.min(100, floor + 50 + Math.floor(rng() * 30))
    else if (name === dump) stats[name] = Math.max(1, floor - 10 + Math.floor(rng() * 15))
    else stats[name] = floor + Math.floor(rng() * 40)
  }
  return stats
}

const NAMES_COMMON = [
  'Pip', 'Dot', 'Bun', 'Fig', 'Mop', 'Twig', 'Puck', 'Nub', 'Wren', 'Kit', 'Roo', 'Dew',
  'Tuft', 'Midge', 'Spud', 'Moss', 'Plop', 'Bean', 'Acorn', 'Pebble', 'Sprout', 'Maple',
  'Clover', 'Pepper', 'Olive', 'Hazel', 'Cricket', 'Mochi', 'Tofu', 'Waffle', 'Nugget',
  'Dumpling', 'Biscuit', 'Noodle', 'Pickles', 'Turnip', 'Basil', 'Ginger', 'Pudding',
  'Peanut', 'Truffle', 'Toffee', 'Fudge', 'Wobble', 'Ziggy', 'Miso', 'Nacho', 'Churro',
  'Pretzel', 'Crouton', 'Poppy', 'Fern', 'Juniper', 'Scout', 'Rascal', 'Gizmo', 'Widget',
  'Pixel', 'Byte', 'Glitch', 'Blip', 'Spark', 'Puddle', 'Drizzle', 'Nimbus', 'Smudge', 'Freckle',
]
const NAMES_RARE = [
  'Ember', 'Flint', 'Storm', 'Rune', 'Onyx', 'Sable', 'Dusk', 'Echo', 'Frost', 'Luna',
  'Zephyr', 'Vesper', 'Corvid', 'Bramble', 'Tempest', 'Indigo', 'Cobalt', 'Jasper', 'Opal',
  'Quartz', 'Slate', 'Aurora', 'Zenith', 'Meridian', 'Cadence', 'Lyric', 'Hemlock', 'Raven',
  'Kestrel', 'Cinder', 'Kindle', 'Torrent', 'Riptide',
]
const NAMES_LEGENDARY = [
  'Motley', 'Axiom', 'Cipher', 'Paradox', 'Catalyst', 'Oracle', 'Vortex', 'Nexus', 'Phantom',
  'Eclipse', 'Chimera', 'Harbinger', 'Aegis', 'Entropy', 'Parallax', 'Theorem', 'Prism',
  'Helix', 'Quasar', 'Nova', 'Maelstrom', 'Leviathan', 'Phoenix', 'Basilisk', 'Kraken', 'Umbra',
]

function rollName(rng: () => number, rarity: Rarity): string {
  if (rarity === 'legendary' || rarity === 'epic') return pick(rng, NAMES_LEGENDARY)
  if (rarity === 'rare' || rarity === 'uncommon') return pick(rng, [...NAMES_COMMON, ...NAMES_RARE])
  return pick(rng, NAMES_COMMON)
}

export function rollBones(seed: string): Bones & { name: string } {
  const rng = mulberry32(hash(seed))
  const rarity = rollRarity(rng)
  const bones: Bones = {
    rarity,
    species: pick(rng, SPECIES),
    eye: pick(rng, EYES),
    hat: rarity === 'common' ? 'none' : pick(rng, HATS),
    shiny: rng() < 0.01,
    stats: rollStats(rng, rarity),
  }
  return { ...bones, name: rollName(rng, rarity) }
}

export function hatch(seed: string, now: number): Soul {
  const { name, species } = rollBones(seed)
  return { seed, name, personality: `a ${species} with strong opinions`, hatchedAt: now }
}

// A buddy hatched by Claude Code before v2.1.97 left its name and personality in
// ~/.claude.json. Its bones came from a hash a mod cannot reproduce, so the species
// is read back from the personality text when it names one.
export function restore(claudeJson: string, now: number): Soul | null {
  let config: any
  try {
    config = JSON.parse(claudeJson)
  } catch {
    return null
  }
  const old = config?.companion
  if (typeof old?.name !== 'string' || !old.name) return null
  const personality = typeof old.personality === 'string' ? old.personality : ''
  const words = personality.toLowerCase().match(/[a-z]+/g) ?? []
  const species = SPECIES.find(s => words.includes(s))
  const seed = String(config.oauthAccount?.accountUuid ?? config.userID ?? old.name)
  return { seed, name: old.name, personality, hatchedAt: Number(old.hatchedAt) || now, ...(species ? { species } : {}) }
}

export function embody(soul: Soul): Companion {
  const { name: _rolledName, ...bones } = rollBones(soul.seed)
  return { ...bones, ...soul, species: soul.species ?? bones.species }
}

// The stat that rises highest above the rest decides the default voice.
export function peakStat(c: Bones): StatName {
  return STAT_NAMES.reduce((a, b) => (c.stats[b] > c.stats[a] ? b : a))
}
