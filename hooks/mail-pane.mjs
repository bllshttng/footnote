const PANE_ID = 'fno-mail-inspector'
const MIN_VERSION = [2, 1, 287]
const POLL_MS = 2_000
const COMMAND_TIMEOUT_MS = 5_000
const CLASSIFICATION_CACHE_LIMIT = 128
const CHAT_WINDOW = 100

let enabled = false
let muxPane = false
let pollInstalled = false
let generation = 0
let selection = null
let view = null
let viewError = ''
let loading = false
let reading = false
let projectionCache = null
let projectionCacheAt = -Infinity
const classificationCache = new Map()

function versionParts(value) {
  const match = /^(\d+)\.(\d+)\.(\d+)/.exec(String(value ?? '').trim())
  return match ? match.slice(1).map(Number) : null
}

function versionSupported(value) {
  const parts = versionParts(value)
  if (!parts) return false
  for (let index = 0; index < MIN_VERSION.length; index += 1) {
    if (parts[index] > MIN_VERSION[index]) return true
    if (parts[index] < MIN_VERSION[index]) return false
  }
  return true
}

function paneNumber(value) {
  let digits = String(value ?? '').trim()
  while (digits.startsWith('%')) digits = digits.slice(1)
  if (!digits) return null
  for (const digit of digits) {
    if (digit < '0' || digit > '9') return null
  }
  const parsed = Number(digits)
  return Number.isSafeInteger(parsed) && parsed >= 0 ? parsed : null
}

function errorText(error) {
  return error instanceof Error ? error.message : String(error)
}

function invalidate($) {
  try {
    $.ui.invalidate('ui.render')
  } catch {
    // A redraw can be refused while a session is unloading.
  }
}

function cacheClassification(key, value) {
  classificationCache.delete(key)
  classificationCache.set(key, value)
  while (classificationCache.size > CLASSIFICATION_CACHE_LIMIT) {
    const oldest = classificationCache.keys().next().value
    classificationCache.delete(oldest)
  }
}

async function classify($, requestId, text) {
  const key = `${requestId}\u0000${text}`
  if (classificationCache.has(key)) return classificationCache.get(key)

  const result = await $.process.run(
    ['fno-agents', 'mail-envelope', '--classify'],
    { stdin: JSON.stringify([text]), timeoutMs: COMMAND_TIMEOUT_MS },
  )
  if (result.exitCode !== 0) {
    throw new Error(result.stderr.trim() || `mail classifier exited ${result.exitCode}`)
  }

  const decoded = JSON.parse(result.stdout)
  const item = Array.isArray(decoded) ? decoded[0] : decoded
  const turns = item?.framing === 'header' && Array.isArray(item.header_turns)
    ? item.header_turns.filter(turn =>
      typeof turn?.id === 'string' &&
      turn.id.startsWith('fmail-') &&
      typeof turn?.sender === 'string' &&
      turn.sender.length > 0)
    : []
  cacheClassification(key, turns)
  return turns
}

function keyPart(value) {
  return String(value).replace(/[^A-Za-z0-9_-]/g, '_')
}

function headerControls($, event, line, turn, requestId) {
  const idAt = line.indexOf(turn.id)
  if (idAt < 0) return null
  const senderLabel = `@${turn.sender}`
  const senderAt = line.lastIndexOf(senderLabel, idAt)
  if (senderAt < 0) return null

  const afterId = line.slice(idAt + turn.id.length)
  if (!afterId.startsWith(' · ')) return null

  let prefix = line.slice(0, senderAt)
  if (prefix.endsWith('`')) prefix = prefix.slice(0, -1)
  const between = line.slice(senderAt + senderLabel.length, idAt)
  let bodyLine = afterId.slice(3)
  const closingFence = bodyLine.indexOf('`')
  if (closingFence >= 0) {
    bodyLine = bodyLine.slice(0, closingFence) + bodyLine.slice(closingFence + 1)
  }

  const { Box, Button, Text } = $.ui.resolve(event)
  const controls = []
  if (prefix) controls.push(Text({ children: [prefix] }))
  controls.push(Button({
    key: `mail-sender-${keyPart(requestId)}-${keyPart(turn.id)}`,
    label: senderLabel,
    plain: true,
    onPress: () => openFromMessage($, turn.id),
  }))
  if (between) controls.push(Text({ children: [between] }))
  controls.push(Button({
    key: `mail-message-${keyPart(requestId)}-${keyPart(turn.id)}`,
    label: turn.id,
    plain: true,
    onPress: () => openMessage($, turn.id),
  }))

  return {
    buttons: Box({ flexDirection: 'row', flexWrap: 'wrap', children: controls }),
    bodyLine,
  }
}

async function runJson($, argv, timeoutMs = COMMAND_TIMEOUT_MS) {
  const result = await $.process.run(argv, { timeoutMs })
  if (result.exitCode !== 0) {
    throw new Error(result.stderr.trim() || `${argv[0]} exited ${result.exitCode}`)
  }
  return JSON.parse(result.stdout)
}

async function readThreads($, force = false) {
  const now = await $.clock.now()
  if (!force && projectionCache && now - projectionCacheAt < 1_000) {
    return projectionCache
  }
  const projection = await runJson($, ['fno-agents', 'mail-threads', '--format', 'json'])
  if (!Array.isArray(projection?.threads) || !Array.isArray(projection?.participants)) {
    throw new Error('mail thread projection has an invalid shape')
  }
  projectionCache = projection
  projectionCacheAt = now
  return projection
}

function messageMatches(projection, messageId) {
  const matches = []
  for (const thread of projection.threads ?? []) {
    for (const row of thread.rows ?? []) {
      if (row.id === messageId) {
        matches.push({
          kind: 'chat',
          chatId: thread.chat_id,
          rows: thread.rows,
          row,
        })
      }
    }
  }
  for (const channel of projection.channels ?? []) {
    for (const row of channel.rows ?? []) {
      if (row.id === messageId) {
        matches.push({
          kind: 'channel',
          scope: channel.scope,
          rows: channel.rows,
          row,
        })
      }
    }
  }
  const announcements = projection.announcements ?? []
  for (const row of announcements) {
    if (row.id === messageId) {
      matches.push({
        kind: 'channel',
        scope: 'announcements',
        rows: announcements,
        row,
      })
    }
  }
  return matches
}

function rowForMessage(projection, messageId) {
  const matches = messageMatches(projection, messageId)
  if (matches.length === 0) throw new Error('Message is no longer available')
  if (matches.length > 1) throw new Error('Message id resolves to more than one thread')
  return matches[0]
}

function sessionForRow(projection, row) {
  if (row.system === true) throw new Error('System messages do not have a peer session')
  const fromKey = typeof row.from_key === 'string' ? row.from_key : ''
  if (!fromKey) throw new Error('Sender identity is unavailable')
  const participant = (projection.participants ?? []).find(item => item.key === fromKey)
  if (!participant || participant.system === true) {
    throw new Error('Sender identity is unavailable')
  }
  const sessionId = typeof participant.session_id === 'string'
    ? participant.session_id.trim()
    : ''
  if (!sessionId) throw new Error('Sender session id is unavailable')
  return sessionId
}

async function readPeek($, sessionId) {
  const result = await $.process.run(
    ['fno', 'agents', 'peek', sessionId, '--lines', '30', '--json'],
    { timeoutMs: COMMAND_TIMEOUT_MS },
  )
  if (result.exitCode === 13) throw new Error('Session is not in the local registry')
  if (result.exitCode === 1) {
    throw new Error(result.stderr.trim() || 'Session transcript is unreadable')
  }
  if (result.exitCode !== 0) {
    throw new Error(result.stderr.trim() || `Peer read exited ${result.exitCode}`)
  }

  const rows = []
  for (const line of result.stdout.split(/\r?\n/)) {
    if (!line.trim()) continue
    let row
    try {
      row = JSON.parse(line)
    } catch {
      throw new Error('Peer read returned invalid JSONL')
    }
    if (typeof row.status === 'string') {
      rows.push({ status: row.status })
    } else if (typeof row.text === 'string') {
      rows.push({
        role: typeof row.role === 'string' ? row.role : 'message',
        text: row.text,
      })
    } else {
      throw new Error('Peer read returned an unknown record')
    }
  }
  return rows.length ? rows : [{ status: 'no activity yet' }]
}

function chatWindow(rows, selectedId) {
  const selectedIndex = rows.findIndex(row => row.id === selectedId)
  if (rows.length <= CHAT_WINDOW) return { rows, start: 0 }
  const desired = selectedIndex < 0 ? rows.length - CHAT_WINDOW : selectedIndex - 49
  const start = Math.max(0, Math.min(desired, rows.length - CHAT_WINDOW))
  return { rows: rows.slice(start, start + CHAT_WINDOW), start }
}

async function refreshSelection($, expectedGeneration = generation) {
  if (!selection || reading || expectedGeneration !== generation) return
  reading = true
  loading = view === null
  try {
    let nextView
    if (selection.kind === 'peer') {
      nextView = {
        kind: 'peer',
        sessionId: selection.sessionId,
        rows: await readPeek($, selection.sessionId),
      }
    } else if (selection.kind === 'chat' || selection.kind === 'channel') {
      const projection = await readThreads($, true)
      const match = rowForMessage(projection, selection.messageId)
      if (match.kind !== selection.kind) throw new Error('Message changed thread type')
      if (selection.kind === 'chat' && match.chatId !== selection.chatId) {
        throw new Error('Message changed chat')
      }
      if (selection.kind === 'channel' && match.scope !== selection.scope) {
        throw new Error('Message changed channel')
      }
      const window = chatWindow(match.rows, selection.messageId)
      nextView = {
        kind: selection.kind,
        chatId: match.chatId,
        scope: match.scope,
        messageId: selection.messageId,
        rows: window.rows,
        start: window.start,
        total: match.rows.length,
      }
    } else {
      nextView = { kind: 'unavailable', message: selection.message }
    }
    if (expectedGeneration === generation) {
      view = nextView
      viewError = ''
    }
  } catch (error) {
    if (expectedGeneration === generation) viewError = errorText(error)
  } finally {
    reading = false
    if (expectedGeneration === generation) {
      loading = false
      invalidate($)
    }
  }
}

async function refreshIfVisible($) {
  if (!enabled || muxPane || !selection || reading) return
  try {
    const panes = await $.ui.panes()
    if (!panes.some(pane => pane.id === PANE_ID && pane.isShown && pane.isPlaced)) return
  } catch {
    return
  }
  await refreshSelection($, generation)
}

async function activate($, nextSelection) {
  generation += 1
  const currentGeneration = generation
  selection = nextSelection
  view = null
  viewError = ''
  loading = true
  projectionCache = null
  try {
    await $.ui.open({
      id: PANE_ID,
      title: nextSelection.title || 'Mail',
      focus: true,
      closeOnEscape: true,
    })
  } catch (error) {
    loading = false
    viewError = errorText(error)
    invalidate($)
    return
  }
  await refreshIfVisible($)
  if (currentGeneration === generation && !view && !viewError) {
    loading = true
    invalidate($)
  }
}

async function unavailable($, messageId, error) {
  await activate($, {
    kind: 'unavailable',
    messageId,
    title: 'Mail unavailable',
    message: errorText(error),
  })
  if (selection?.messageId === messageId) {
    view = { kind: 'unavailable', message: errorText(error) }
    viewError = ''
    loading = false
    invalidate($)
  }
}

async function openFromMessage($, messageId) {
  try {
    const projection = await readThreads($)
    const match = rowForMessage(projection, messageId)
    const sessionId = sessionForRow(projection, match.row)
    await activate($, {
      kind: 'peer',
      messageId,
      sessionId,
      title: `Session ${sessionId}`,
    })
  } catch (error) {
    await unavailable($, messageId, error)
  }
}

async function openMessage($, messageId) {
  try {
    const projection = await readThreads($)
    const match = rowForMessage(projection, messageId)
    await activate($, {
      kind: match.kind,
      messageId,
      chatId: match.chatId,
      scope: match.scope,
      title: match.kind === 'chat' ? `Chat ${match.chatId}` : `Channel ${match.scope}`,
    })
  } catch (error) {
    await unavailable($, messageId, error)
  }
}

function peerElements(Text, target, currentView, currentError, isLoading) {
  const children = [
    Text({ bold: true, children: [`Session ${target.sessionId}`] }),
    Text({ dimColor: true, wrap: 'wrap', children: [`fno agents peek ${target.sessionId}`] }),
  ]
  if (currentError) {
    children.push(Text({ color: 'yellow', wrap: 'wrap', children: [currentView ? `stale: ${currentError}` : `unavailable: ${currentError}`] }))
  }
  if (isLoading && !currentView) children.push(Text({ dimColor: true, children: ['Loading recent activity…'] }))
  if (currentView?.kind === 'peer') {
    for (const row of currentView.rows) {
      if (row.status === 'no activity yet') {
        children.push(Text({ dimColor: true, children: ['No activity yet'] }))
      } else if (row.status) {
        children.push(Text({ dimColor: true, wrap: 'wrap', children: [row.status] }))
      } else {
        const label = `${row.role}: ${row.text}`
        const clipped = label.length > 8_000 ? `${label.slice(0, 8_000)}…` : label
        children.push(Text({ wrap: 'wrap', children: [clipped] }))
      }
    }
  }
  return children
}

function threadElements(Text, target, currentView, currentError, isLoading) {
  const chat = target.kind === 'chat'
  const children = [
    Text({ bold: true, children: [chat ? `Chat ${target.chatId}` : `Channel ${target.scope}`] }),
  ]
  if (currentView?.total > currentView.rows.length) {
    const first = currentView.start + 1
    const last = currentView.start + currentView.rows.length
    children.push(Text({ dimColor: true, children: [`Showing ${first}–${last} of ${currentView.total} messages`] }))
  }
  if (currentError) {
    children.push(Text({ color: 'yellow', wrap: 'wrap', children: [currentView ? `stale: ${currentError}` : `unavailable: ${currentError}`] }))
  }
  if (isLoading && !currentView) children.push(Text({ dimColor: true, children: ['Loading conversation…'] }))
  if (currentView && (currentView.kind === 'chat' || currentView.kind === 'channel')) {
    for (const row of currentView.rows) {
      const selected = row.id === target.messageId
      const sender = typeof row.from === 'string' ? row.from : 'Unknown sender'
      const timestamp = typeof row.ts === 'string' ? row.ts : ''
      const body = typeof row.body === 'string' ? row.body : ''
      children.push(Text({ bold: selected, wrap: 'wrap', children: [`${selected ? '› ' : ''}${sender}${timestamp ? ` · ${timestamp}` : ''}`] }))
      children.push(Text({ wrap: 'wrap', children: [body.length > 8_000 ? `${body.slice(0, 8_000)}…` : body] }))
    }
  }
  return children
}

function renderPane($, event) {
  const { Box, Text } = $.ui.resolve(event)
  if (!selection) {
    return Box({
      flexDirection: 'column',
      children: [Text({ dimColor: true, wrap: 'wrap', children: ['Select a sender or fmail id in a delivered header to open it here.'] })],
    })
  }
  if (selection.kind === 'unavailable') {
    return Box({
      flexDirection: 'column',
      children: [Text({ color: 'yellow', wrap: 'wrap', children: [selection.message] })],
    })
  }
  const children = selection.kind === 'peer'
    ? peerElements(Text, selection, view, viewError, loading)
    : threadElements(Text, selection, view, viewError, loading)
  return Box({ flexDirection: 'column', gap: 1, children })
}

export function registerMailPane(on) {
  on('session.start', async ($, event, next) => {
    try {
      const version = await $.session.version()
      const server = await $.env.get('FNO_SERVER')
      const pane = await $.env.get('FNO_PANE')
      enabled = versionSupported(version?.version)
      muxPane = paneNumber(pane) !== null
      // FNO_SERVER can exist without a pane-bound caller; only FNO_PANE suppresses the fallback.
      void server
    } catch {
      enabled = false
      muxPane = false
    }
    if (!pollInstalled) {
      pollInstalled = true
      $.clock.every(POLL_MS, () => refreshIfVisible($))
    }
    return next(event)
  })

  on('session.end', async ($, event, next) => {
    generation += 1
    selection = null
    view = null
    viewError = ''
    loading = false
    projectionCache = null
    projectionCacheAt = -Infinity
    classificationCache.clear()
    return next(event)
  })

  on('ui.close', async ($, event, next) => {
    if (event.id === PANE_ID) {
      generation += 1
      selection = null
      view = null
      viewError = ''
      loading = false
      projectionCache = null
      projectionCacheAt = -Infinity
    }
    return next(event)
  })

  on('ui.render', { component: 'UserMessage' }, async ($, event, next) => {
    if (!enabled || muxPane) return next(event)
    const text = typeof event.props?.text === 'string' ? event.props.text : ''
    // This candidate check avoids a process for ordinary prompts; Rust still decides whether it is mail.
    if (!text.includes('fmail-')) return next(event)

    let turns
    try {
      turns = await classify($, event.requestId, text)
    } catch {
      return next(event)
    }
    if (!turns.length) return next(event)

    const { Box, Text } = $.ui.resolve(event)
    const pending = [...turns]
    const children = []
    let plainLines = []
    let hasButtons = false
    const flushPlain = () => {
      if (!plainLines.length) return
      children.push(Text({ wrap: 'wrap', children: [plainLines.join('\n')] }))
      plainLines = []
    }
    for (const line of text.split('\n')) {
      let index = -1
      let result = null
      for (let candidate = 0; candidate < pending.length; candidate += 1) {
        if (!line.includes(pending[candidate].id)) continue
        const parsed = headerControls($, event, line, pending[candidate], event.requestId)
        if (!parsed) continue
        index = candidate
        result = parsed
        break
      }
      if (index < 0 || !result) {
        plainLines.push(line)
        continue
      }
      pending.splice(index, 1)
      flushPlain()
      children.push(result.buttons)
      if (result.bodyLine) {
        children.push(Text({ wrap: 'wrap', children: [result.bodyLine] }))
      }
      hasButtons = true
    }
    flushPlain()
    if (!hasButtons) return next(event)
    return Box({
      flexDirection: 'column',
      children,
    })
  })

  on('ui.render', { component: 'Pane' }, async ($, event, next) => {
    if (event.requestId !== PANE_ID || !enabled || muxPane) return next(event)
    if (selection && !view && !loading) await refreshSelection($, generation)
    return renderPane($, event)
  })
}
