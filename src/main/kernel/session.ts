/**
 * A single running kernel: its process, its five ZMQ channels, and the
 * request/reply bookkeeping on top of them.
 *
 * This is the piece that lets Herse be a real desktop app rather than a browser
 * wrapper — there is no `jupyter server` in the picture. We spawn the kernel
 * from its kernelspec and speak the wire protocol to it directly.
 */

import { EventEmitter } from 'node:events'
import { type ChildProcess, spawn } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import { userInfo } from 'node:os'
import * as zmq from 'zeromq'

import type { JupyterMessage, KernelSpec, KernelStatus, SessionInfo } from '@shared/jupyter'
import { type ConnectionInfo, channelUrl, createConnectionInfo, removeConnectionFile, writeConnectionFile } from './connection'
import { type SigningConfig, createHeader, decode, encode } from './wire'

export type Channel = 'shell' | 'iopub' | 'stdin' | 'control'

export interface SessionEvents {
  message: [{ channel: Channel; message: JupyterMessage }]
  status: [{ status: KernelStatus; reason?: string }]
}

interface PendingReply {
  resolve: (message: JupyterMessage) => void
  reject: (error: Error) => void
  timer: NodeJS.Timeout
}

/** How long a shell request waits for its reply before we give up on it. */
const REQUEST_TIMEOUT_MS = 60_000
/** kernel_info handshake: how long to keep retrying before declaring failure. */
const STARTUP_TIMEOUT_MS = 30_000
const HEARTBEAT_INTERVAL_MS = 3_000
const HEARTBEAT_TIMEOUT_MS = 5_000
const MISSED_BEATS_BEFORE_DEAD = 3

export class KernelSession extends EventEmitter<SessionEvents> {
  readonly id = randomUUID()
  readonly zmqSession = randomUUID()

  private readonly username = safeUsername()
  private connection!: ConnectionInfo
  private connectionFile!: string
  private signing!: SigningConfig

  private shell!: zmq.Dealer
  private control!: zmq.Dealer
  private iopub!: zmq.Subscriber
  private stdin!: zmq.Dealer
  private heartbeat?: zmq.Request

  private process: ChildProcess | null = null
  private pending = new Map<string, PendingReply>()
  private heartbeatTimer?: NodeJS.Timeout
  private missedBeats = 0
  private disposed = false
  private restarting = false

  private _status: KernelStatus = 'starting'
  private startedAt = Date.now()
  /** Set once the kernel answers kernel_info_request. */
  private languageName: string

  constructor(
    readonly spec: KernelSpec,
    readonly cwd: string,
  ) {
    super()
    this.languageName = spec.language
  }

  get status(): KernelStatus {
    return this._status
  }

  info(): SessionInfo {
    return {
      id: this.id,
      kernelName: this.spec.name,
      displayName: this.spec.display_name,
      language: this.languageName,
      status: this._status,
      cwd: this.cwd,
      pid: this.process?.pid ?? null,
      startedAt: this.startedAt,
    }
  }

  /* ------------------------------------------------------------------ */
  /* Lifecycle                                                           */
  /* ------------------------------------------------------------------ */

  async start(): Promise<void> {
    this.connection = await createConnectionInfo(this.spec.name)
    this.connectionFile = await writeConnectionFile(this.connection)
    this.signing = { key: this.connection.key, scheme: this.connection.signature_scheme }

    this.openSockets()
    this.spawnProcess()

    // Sockets are connected, but a SUB socket's subscription only takes effect
    // once the kernel has accepted it. Polling kernel_info until we get a reply
    // is the standard way to know the whole pipe is live — without it the first
    // cell's output is routinely dropped.
    await this.awaitKernelInfo()

    this.setStatus('idle')
    this.startHeartbeat()
  }

  private openSockets(): void {
    const routingId = this.zmqSession

    this.shell = new zmq.Dealer({ routingId })
    this.control = new zmq.Dealer({ routingId })
    this.stdin = new zmq.Dealer({ routingId })
    this.iopub = new zmq.Subscriber()

    this.shell.connect(channelUrl(this.connection, this.connection.shell_port))
    this.control.connect(channelUrl(this.connection, this.connection.control_port))
    this.stdin.connect(channelUrl(this.connection, this.connection.stdin_port))
    this.iopub.connect(channelUrl(this.connection, this.connection.iopub_port))
    this.iopub.subscribe() // all topics

    void this.pump(this.shell, 'shell')
    void this.pump(this.control, 'control')
    void this.pump(this.stdin, 'stdin')
    void this.pump(this.iopub, 'iopub')
  }

  private spawnProcess(): void {
    const argv = this.spec.argv.map((arg) =>
      arg
        .replace('{connection_file}', this.connectionFile)
        .replace('{resource_dir}', this.spec.resource_dir ?? ''),
    )

    const child = spawn(argv[0], argv.slice(1), {
      cwd: this.cwd,
      env: {
        ...process.env,
        ...this.spec.env,
        JPY_PARENT_PID: String(process.pid),
        // Unbuffered stdio so kernel-side crash messages reach us promptly.
        PYTHONUNBUFFERED: '1',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    })

    child.on('error', (error) => {
      this.setStatus('dead', `Failed to launch kernel: ${error.message}`)
    })

    child.on('exit', (code, signal) => {
      this.process = null
      if (this.disposed || this.restarting) return
      const reason =
        signal != null ? `Kernel terminated with signal ${signal}` : `Kernel exited with code ${code}`
      this.setStatus('dead', reason)
    })

    // Kernel stderr is where tracebacks from a failed boot show up.
    child.stderr?.on('data', (chunk: Buffer) => {
      process.stderr.write(`[kernel ${this.spec.name}] ${chunk.toString()}`)
    })

    this.process = child
  }

  private async awaitKernelInfo(): Promise<void> {
    const deadline = Date.now() + STARTUP_TIMEOUT_MS

    while (Date.now() < deadline) {
      if (this.disposed) throw new Error('Session disposed during startup')
      if (this.process === null && this._status === 'dead') {
        throw new Error('Kernel process exited during startup')
      }

      try {
        const reply = await this.request('shell', 'kernel_info_request', {}, 2_000)
        const content = reply.content as { language_info?: { name?: string } }
        if (content.language_info?.name) this.languageName = content.language_info.name
        return
      } catch {
        // Kernel isn't listening yet — retry until the deadline.
      }
    }

    throw new Error(`Kernel "${this.spec.display_name}" did not respond within ${STARTUP_TIMEOUT_MS / 1000}s`)
  }

  async dispose(): Promise<void> {
    if (this.disposed) return
    this.disposed = true
    this.stopHeartbeat()

    // Ask politely on the control channel, then make sure.
    try {
      await this.request('control', 'shutdown_request', { restart: false }, 2_000)
    } catch {
      // Kernel is unresponsive; fall through to the kill.
    }

    this.killProcess()
    this.closeSockets()
    await removeConnectionFile(this.connectionFile)

    for (const { reject, timer } of this.pending.values()) {
      clearTimeout(timer)
      reject(new Error('Kernel session closed'))
    }
    this.pending.clear()
    this.setStatus('dead')
  }

  /**
   * Kill and relaunch against the same connection file. Reusing the ports means
   * our sockets simply reconnect, so the session id — and every reference the
   * renderer holds to it — stays valid.
   */
  async restart(): Promise<void> {
    if (this.disposed) throw new Error('Cannot restart a disposed session')

    this.restarting = true
    this.stopHeartbeat()
    this.setStatus('restarting')

    try {
      await this.request('control', 'shutdown_request', { restart: true }, 2_000).catch(() => {})
      this.killProcess()
      await waitForExit(this.process)

      for (const { reject, timer } of this.pending.values()) {
        clearTimeout(timer)
        reject(new Error('Kernel restarted'))
      }
      this.pending.clear()

      this.startedAt = Date.now()
      this.spawnProcess()
      await this.awaitKernelInfo()
      this.setStatus('idle')
      this.startHeartbeat()
    } finally {
      this.restarting = false
    }
  }

  private killProcess(): void {
    const child = this.process
    if (!child || child.killed) return
    child.kill('SIGTERM')
    // Escalate if the kernel ignores SIGTERM.
    const pid = child.pid
    setTimeout(() => {
      if (pid && this.process?.pid === pid) {
        try {
          process.kill(pid, 'SIGKILL')
        } catch {
          // Already gone.
        }
      }
    }, 3_000).unref()
  }

  private closeSockets(): void {
    for (const socket of [this.shell, this.control, this.stdin, this.iopub, this.heartbeat]) {
      try {
        socket?.close()
      } catch {
        // Already closed.
      }
    }
  }

  /* ------------------------------------------------------------------ */
  /* Messaging                                                           */
  /* ------------------------------------------------------------------ */

  private socketFor(channel: Exclude<Channel, 'iopub'>): zmq.Dealer {
    if (channel === 'shell') return this.shell
    if (channel === 'control') return this.control
    return this.stdin
  }

  /** Fire-and-forget send; returns the msg_id so the caller can correlate. */
  async send(
    channel: Exclude<Channel, 'iopub'>,
    msgType: string,
    content: unknown,
  ): Promise<string> {
    const header = createHeader(msgType, this.zmqSession, this.username)
    const frames = encode({ header, content, signing: this.signing })
    await this.socketFor(channel).send(frames)
    return header.msg_id
  }

  /** Send and wait for the reply whose parent_header.msg_id matches. */
  async request(
    channel: Exclude<Channel, 'iopub'>,
    msgType: string,
    content: unknown,
    timeoutMs = REQUEST_TIMEOUT_MS,
  ): Promise<JupyterMessage> {
    const header = createHeader(msgType, this.zmqSession, this.username)
    const frames = encode({ header, content, signing: this.signing })

    const reply = new Promise<JupyterMessage>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(header.msg_id)
        reject(new Error(`Timed out waiting for reply to ${msgType}`))
      }, timeoutMs)
      timer.unref()
      this.pending.set(header.msg_id, { resolve, reject, timer })
    })

    await this.socketFor(channel).send(frames)
    return reply
  }

  private async pump(socket: zmq.Dealer | zmq.Subscriber, channel: Channel): Promise<void> {
    try {
      for await (const frames of socket) {
        let message: JupyterMessage
        try {
          message = decode(frames as Buffer[], this.signing)
        } catch (error) {
          // A bad signature means someone else is talking to our ports. Drop it
          // rather than letting it reach the renderer.
          process.stderr.write(`[herse] dropped kernel message on ${channel}: ${String(error)}\n`)
          continue
        }

        this.settle(message)
        this.trackStatus(channel, message)
        this.emit('message', { channel, message })
      }
    } catch {
      // Socket closed while iterating — expected during shutdown.
    }
  }

  private settle(message: JupyterMessage): void {
    const parentId = message.parent_header?.msg_id
    if (!parentId) return
    // Only *_reply messages resolve a request; iopub echoes share the parent id.
    if (!message.header.msg_type.endsWith('_reply')) return

    const waiter = this.pending.get(parentId)
    if (!waiter) return
    this.pending.delete(parentId)
    clearTimeout(waiter.timer)
    waiter.resolve(message)
  }

  private trackStatus(channel: Channel, message: JupyterMessage): void {
    if (channel !== 'iopub' || message.header.msg_type !== 'status') return
    const state = (message.content as { execution_state?: string }).execution_state
    if (state === 'busy' || state === 'idle') this.setStatus(state)
  }

  private setStatus(status: KernelStatus, reason?: string): void {
    if (this._status === status && !reason) return
    this._status = status
    this.emit('status', { status, reason })
  }

  /* ------------------------------------------------------------------ */
  /* Heartbeat                                                           */
  /* ------------------------------------------------------------------ */

  private startHeartbeat(): void {
    this.stopHeartbeat()
    this.missedBeats = 0
    this.heartbeatTimer = setInterval(() => void this.beat(), HEARTBEAT_INTERVAL_MS)
    this.heartbeatTimer.unref()
  }

  private stopHeartbeat(): void {
    if (this.heartbeatTimer) clearInterval(this.heartbeatTimer)
    this.heartbeatTimer = undefined
    try {
      this.heartbeat?.close()
    } catch {
      // Already closed.
    }
    this.heartbeat = undefined
  }

  private async beat(): Promise<void> {
    if (this.disposed || this.restarting) return

    try {
      if (!this.heartbeat) {
        this.heartbeat = new zmq.Request({
          receiveTimeout: HEARTBEAT_TIMEOUT_MS,
          sendTimeout: HEARTBEAT_TIMEOUT_MS,
          linger: 0,
        })
        this.heartbeat.connect(channelUrl(this.connection, this.connection.hb_port))
      }

      await this.heartbeat.send('ping')
      await this.heartbeat.receive()
      this.missedBeats = 0
    } catch {
      this.missedBeats += 1
      // A REQ socket that timed out is stuck mid-cycle; discard it.
      try {
        this.heartbeat?.close()
      } catch {
        // Already closed.
      }
      this.heartbeat = undefined

      if (this.missedBeats >= MISSED_BEATS_BEFORE_DEAD) {
        this.stopHeartbeat()
        this.setStatus('dead', 'Kernel stopped responding to heartbeats')
      }
    }
  }

  /* ------------------------------------------------------------------ */
  /* High-level operations                                               */
  /* ------------------------------------------------------------------ */

  async execute(code: string): Promise<string> {
    return this.send('shell', 'execute_request', {
      code,
      silent: false,
      store_history: true,
      user_expressions: {},
      allow_stdin: true,
      stop_on_error: true,
    })
  }

  async complete(code: string, cursorPos: number): Promise<JupyterMessage> {
    return this.request('shell', 'complete_request', { code, cursor_pos: cursorPos }, 5_000)
  }

  async inspect(code: string, cursorPos: number, detailLevel: 0 | 1 = 0): Promise<JupyterMessage> {
    return this.request(
      'shell',
      'inspect_request',
      { code, cursor_pos: cursorPos, detail_level: detailLevel },
      5_000,
    )
  }

  async interrupt(): Promise<void> {
    if (this.spec.interrupt_mode === 'message') {
      await this.send('control', 'interrupt_request', {})
      return
    }
    // Signal mode: SIGINT is what raises KeyboardInterrupt inside the kernel.
    if (this.process?.pid) {
      try {
        process.kill(this.process.pid, 'SIGINT')
      } catch {
        // Kernel already exited.
      }
    }
  }

  async sendInput(value: string): Promise<void> {
    await this.send('stdin', 'input_reply', { value })
  }
}

function safeUsername(): string {
  try {
    return userInfo().username
  } catch {
    return 'herse'
  }
}

function waitForExit(child: ChildProcess | null, timeoutMs = 5_000): Promise<void> {
  if (!child || child.exitCode !== null) return Promise.resolve()
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, timeoutMs)
    timer.unref()
    child.once('exit', () => {
      clearTimeout(timer)
      resolve()
    })
  })
}
