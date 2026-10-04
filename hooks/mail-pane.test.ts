import { expect, mock, test } from 'claude-code/testing'

const PANE_ID = 'fno-mail-inspector'
const REQUEST_ID = 'user-message-1'
const MESSAGE_ID = 'fmail-12ab34cd56ef'
const SECOND_MESSAGE_ID = 'fmail-9876abcd5432'
const SYSTEM_MESSAGE_ID = 'fmail-223344556677'
const UNKNOWN_MESSAGE_ID = 'fmail-abcdef123456'
const SESSION_ID = 'session-from-identity'
const CHAT_ID = 'chat-0123456789abcdef'
const HEADER = `\`@old-label · ${MESSAGE_ID} · hello from the worker\``

function userMessage(text: string, requestId = REQUEST_ID) {
  return {
    plugin: 'fno',
    component: 'UserMessage',
    surface: 'terminal',
    requestId,
    viewport: { columns: 120, rows: 40 },
    props: {
      text,
      origin: { kind: 'peer', sessionId: 'wrong-origin-session' },
      isExpanded: true,
      from: { name: 'old-label' },
    },
  } as const
}

function pane() {
  return {
    plugin: 'fno',
    component: 'Pane',
    surface: 'terminal',
    requestId: PANE_ID,
    viewport: { columns: 120, rows: 40 },
    props: {
      title: 'Mail',
      isFocused: true,
      bodyColumns: 60,
      placement: 'dock',
      scroll: { offset: 0, bodyRows: 30 },
      view: {},
    },
  } as const
}

test('delivered sender and message buttons resolve the canonical session and thread', async ($, on) => {
  const clock = mock.clock(on)
  let version = '2.1.288'
  let paneIsOpen = false
  let paneOpen: unknown
  const environment = new Map<string, string>([
    ['FNO_SERVER', '/tmp/fno-server.sock'],
    ['FNO_PANE', ''],
  ])
  const commands: string[][] = []
  let peekExitCode = 0
  const thread = {
    participants: [
      { key: SESSION_ID, session_id: SESSION_ID, name: 'new-label' },
      { key: 'viewer-session', session_id: 'viewer-session', name: 'new-label' },
      { key: 'fno/fleet-incident', session_id: null, name: 'fno/fleet-incident', system: true },
    ],
    threads: [
      {
        chat_id: CHAT_ID,
        participants: [SESSION_ID, 'viewer-session'],
        rows: [
          {
            id: MESSAGE_ID,
            from_key: SESSION_ID,
            to_key: 'viewer-session',
            from: 'new-label',
            to: 'viewer',
            ts: '2026-10-03T20:00:00Z',
            summary: 'hello from the worker',
            body: 'selected chat body',
            system: false,
          },
          {
            id: SECOND_MESSAGE_ID,
            from_key: SESSION_ID,
            to_key: 'viewer-session',
            from: 'new-label',
            to: 'viewer',
            ts: '2026-10-03T20:01:00Z',
            summary: 'second header',
            body: 'second chat body',
            system: false,
          },
          {
            id: SYSTEM_MESSAGE_ID,
            from_key: 'fno/fleet-incident',
            to_key: 'viewer-session',
            from: 'fno/fleet-incident',
            to: 'viewer',
            ts: '2026-10-03T20:02:00Z',
            summary: 'incident notice',
            body: 'incident body',
            system: true,
          },
        ],
      },
    ],
    channels: [],
    announcements: [],
  }

  on('session.start', () => ({ cwd: '/work' }))
  on('session.version', () => ({ value: { version, base: version, builtAt: '' } }))
  on('env.get', ($, e) => ({ value: environment.get(e.name) ?? '' }))
  on('process.run', ($, e) => {
    commands.push(e.argv)
    if (e.argv[0] === 'fno-agents' && e.argv[1] === 'mail-envelope') {
      const texts = JSON.parse(e.init?.stdin ?? '[]') as string[]
      return {
        value: {
          exitCode: 0,
          stderr: '',
          stdout: JSON.stringify(texts.map(text => {
            const labels = new Map([
              [MESSAGE_ID, 'old-label'],
              [SECOND_MESSAGE_ID, 'old-label'],
              [SYSTEM_MESSAGE_ID, 'fno/fleet-incident'],
              [UNKNOWN_MESSAGE_ID, 'old-label'],
            ])
            const header_turns = [...labels]
              .filter(([id]) => text.includes(id))
              .map(([id, sender]) => ({ id, sender }))
            return header_turns.length
              ? { framing: 'header', msg_id: header_turns[0].id, header_turns }
              : { framing: 'bare', msg_id: null, header_turns: [] }
          })),
        },
      }
    }
    if (e.argv[0] === 'fno-agents' && e.argv[1] === 'mail-threads') {
      return { value: { exitCode: 0, stdout: JSON.stringify(thread), stderr: '' } }
    }
    if (e.argv[0] === 'fno' && e.argv[1] === 'agents' && e.argv[2] === 'peek') {
      return {
        value: {
          exitCode: peekExitCode,
          stdout: peekExitCode === 0 ? '{"role":"assistant","text":"peer turn body"}\n' : '',
          stderr: peekExitCode === 13 ? 'unknown session' : '',
        },
      }
    }
    return { value: { exitCode: 1, stdout: '', stderr: 'unexpected command' } }
  })
  on('ui.open', ($, e) => {
    paneIsOpen = true
    paneOpen = e
    return { value: { isPlaced: true } }
  })
  on('ui.close', ($, e) => {
    if (e.id === PANE_ID) paneIsOpen = false
    return { value: undefined }
  })
  on('ui.panes', () => ({ value: paneIsOpen ? [{ id: PANE_ID, title: 'Mail', isShown: true, isPlaced: true }] : [] }))
  on('ui.invalidate', () => ({ value: undefined }))
  on('ui.render', ($, e) => ({ type: 'Text', props: {}, children: [String(e.props?.text ?? 'original message')] }))

  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })
  const plain = await $.ui.mount(userMessage('ordinary prompt without a mail id', 'user-plain'))
  expect(await plain.find({ type: 'Button' })).toBeUndefined()
  expect(commands.filter(argv => argv[0] === 'fno-agents' && argv[1] === 'mail-envelope')).toHaveLength(0)
  await plain.unmount()

  const messageText = `2 held messages · sent 20:00 · held 1m\n${HEADER}\noriginal mail body\n\`@old-label · ${SECOND_MESSAGE_ID} · second header\`\nsecond mail body`
  const message = await $.ui.mount(userMessage(messageText))
  const senderKey = `mail-sender-${REQUEST_ID}-${MESSAGE_ID}`
  const messageKey = `mail-message-${REQUEST_ID}-${MESSAGE_ID}`
  const secondMessageKey = `mail-message-${REQUEST_ID}-${SECOND_MESSAGE_ID}`
  if (!await message.find({ key: senderKey })) throw new Error('sender button missing')
  if (!await message.find({ key: messageKey })) throw new Error('message button missing')
  if (!await message.find({ key: secondMessageKey })) throw new Error('second held message button missing')
  if (!await message.find({ type: 'Text', text: /original mail body/ })) throw new Error('mail body was not preserved')
  if (!await message.find({ type: 'Text', text: /second mail body/ })) throw new Error('second held body was not preserved')
  const messageTree = await message.find({ type: 'Box' })
  const messageLayout = JSON.stringify(messageTree)
  const heldSummaryAt = messageLayout.indexOf('2 held messages')
  const firstControlAt = messageLayout.indexOf(senderKey)
  const firstBodyAt = messageLayout.indexOf('original mail body')
  const secondControlAt = messageLayout.indexOf(secondMessageKey)
  const secondBodyAt = messageLayout.indexOf('second mail body')
  if (
    !(heldSummaryAt < firstControlAt && firstControlAt < firstBodyAt &&
      firstBodyAt < secondControlAt && secondControlAt < secondBodyAt)
  ) {
    throw new Error('held-mail controls were not kept beside their message bodies')
  }

  await message.press({ key: senderKey })
  expect(paneOpen).toMatchObject({ id: PANE_ID, focus: true, closeOnEscape: true })
  expect(commands.some(argv => argv[0] === 'fno' && argv[1] === 'agents' && argv[2] === 'peek' && argv[3] === SESSION_ID)).toBe(true)
  const peerPane = await $.ui.mount(pane())
  if (!await peerPane.find({ type: 'Text', text: /peer turn body/ })) throw new Error('peer transcript was not rendered')
  await peerPane.unmount()

  await message.press({ key: secondMessageKey })
  const chatPane = await $.ui.mount(pane())
  if (!await chatPane.find({ type: 'Text', text: /second chat body/ })) throw new Error('chat body was not rendered')
  if (!await chatPane.find({ type: 'Text', text: /› new-label/ })) throw new Error('selected message was not marked')
  await chatPane.unmount()

  paneIsOpen = false
  const beforeClosedTick = commands.length
  await clock.advance(2_000)
  expect(commands).toHaveLength(beforeClosedTick)
  await message.unmount()

  const systemMessage = await $.ui.mount(userMessage(
    `\`@fno/fleet-incident · ${SYSTEM_MESSAGE_ID} · incident notice\`\nincident body`,
    'user-system-message',
  ))
  await systemMessage.press({ key: `mail-sender-user-system-message-${SYSTEM_MESSAGE_ID}` })
  const systemPane = await $.ui.mount(pane())
  if (!await systemPane.find({ type: 'Text', text: /System messages do not have a peer session/ })) {
    throw new Error('system sender did not show an unavailable state')
  }
  expect(commands.some(argv => argv[0] === 'fno' && argv[1] === 'agents' && argv[2] === 'peek' && argv[3] === 'fno/fleet-incident')).toBe(false)
  await systemPane.unmount()
  paneIsOpen = false
  await systemMessage.unmount()

  const unknownMessage = await $.ui.mount(userMessage(
    `\`@old-label · ${UNKNOWN_MESSAGE_ID} · unknown message\`\nunknown body`,
    'user-unknown-message',
  ))
  await unknownMessage.press({ key: `mail-message-user-unknown-message-${UNKNOWN_MESSAGE_ID}` })
  const unknownPane = await $.ui.mount(pane())
  if (!await unknownPane.find({ type: 'Text', text: /Message is no longer available/ })) {
    throw new Error('unknown message did not show an unavailable state')
  }
  await unknownPane.unmount()
  paneIsOpen = false
  await unknownMessage.unmount()

  const malformed = await $.ui.mount(userMessage('`@old-label · msg-123 · legacy header`\nplain body', 'user-malformed'))
  if (await malformed.find({ type: 'Button' })) throw new Error('malformed legacy header gained controls')
  if (!await malformed.find({ type: 'Text', text: /legacy header/ })) throw new Error('malformed header text was not preserved')
  await malformed.unmount()

  peekExitCode = 13
  const unreadable = await $.ui.mount(userMessage(`${HEADER}\noriginal mail body`, 'user-unreadable'))
  await unreadable.press({ key: `mail-sender-user-unreadable-${MESSAGE_ID}` })
  const unreadablePane = await $.ui.mount(pane())
  if (!await unreadablePane.find({ type: 'Text', text: /Session is not in the local registry/ })) {
    throw new Error('unreadable peer was not reported')
  }
  expect(commands.some(argv => argv[0] === 'fno' && argv[1] === 'agents' && argv[2] === 'peek' && argv[3] === 'old-label')).toBe(false)
  await unreadablePane.unmount()
  paneIsOpen = false
  await unreadable.unmount()
  peekExitCode = 0

  version = '2.1.286'
  environment.set('FNO_PANE', '')
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })
  const oldEngine = await $.ui.mount(userMessage(`${HEADER}\noriginal mail body`, 'user-old-engine'))
  expect(await oldEngine.find({ key: senderKey })).toBeUndefined()
  await oldEngine.unmount()

  version = '2.1.288'
  environment.set('FNO_PANE', '%17')
  await $.session.start({ surface: 'terminal', isInteractive: true, cwd: '/work' })
  const muxPane = await $.ui.mount(userMessage(`${HEADER}\noriginal mail body`, 'user-mux-pane'))
  expect(await muxPane.find({ key: senderKey })).toBeUndefined()
  await muxPane.unmount()
})
