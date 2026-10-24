/**
 * Jupyter wire protocol v5.3 framing.
 *
 * A message on the wire is:
 *   [zmq identities...] <IDS|MSG> <hmac> <header> <parent> <metadata> <content> [buffers...]
 *
 * The HMAC covers exactly the four JSON frames, in order, and nothing else.
 */

import { createHmac, randomUUID, timingSafeEqual } from 'node:crypto'
import type { JupyterMessage, MessageHeader } from '@shared/jupyter'

export const DELIMITER = '<IDS|MSG>'
const PROTOCOL_VERSION = '5.3'

const SCHEME_TO_HASH: Record<string, string> = {
  'hmac-sha256': 'sha256',
  'hmac-sha1': 'sha1',
  'hmac-sha512': 'sha512',
}

export interface SigningConfig {
  key: string
  scheme: string
}

function sign(frames: Buffer[], { key, scheme }: SigningConfig): string {
  // An empty key means the kernel has signing disabled.
  if (!key) return ''
  const hash = SCHEME_TO_HASH[scheme] ?? 'sha256'
  const hmac = createHmac(hash, key)
  for (const frame of frames) hmac.update(frame)
  return hmac.digest('hex')
}

export function createHeader(msgType: string, session: string, username: string): MessageHeader {
  return {
    msg_id: randomUUID(),
    session,
    username,
    date: new Date().toISOString(),
    msg_type: msgType,
    version: PROTOCOL_VERSION,
  }
}

export interface EncodeOptions {
  header: MessageHeader
  parentHeader?: Partial<MessageHeader>
  metadata?: Record<string, unknown>
  content?: unknown
  buffers?: Buffer[]
  signing: SigningConfig
}

export function encode({
  header,
  parentHeader = {},
  metadata = {},
  content = {},
  buffers = [],
  signing,
}: EncodeOptions): Buffer[] {
  const jsonFrames = [
    Buffer.from(JSON.stringify(header)),
    Buffer.from(JSON.stringify(parentHeader)),
    Buffer.from(JSON.stringify(metadata)),
    Buffer.from(JSON.stringify(content)),
  ]
  return [Buffer.from(DELIMITER), Buffer.from(sign(jsonFrames, signing)), ...jsonFrames, ...buffers]
}

export class SignatureError extends Error {}

/**
 * Parse frames received on any channel. SUB sockets prefix a topic frame and
 * ROUTER sockets prefix identity frames, so we locate the delimiter rather than
 * assuming a fixed offset.
 */
export function decode(frames: Buffer[], signing: SigningConfig): JupyterMessage {
  const delimIndex = frames.findIndex((f) => f.toString() === DELIMITER)
  if (delimIndex === -1) throw new Error('Malformed kernel message: no <IDS|MSG> delimiter')

  const signature = frames[delimIndex + 1]
  const jsonFrames = frames.slice(delimIndex + 2, delimIndex + 6)
  if (jsonFrames.length < 4) throw new Error('Malformed kernel message: missing JSON frames')

  if (signing.key) {
    const expected = Buffer.from(sign(jsonFrames, signing))
    // Length check first: timingSafeEqual throws on mismatched lengths.
    if (expected.length !== signature.length || !timingSafeEqual(expected, signature)) {
      throw new SignatureError('Kernel message signature mismatch')
    }
  }

  const [header, parentHeader, metadata, content] = jsonFrames.map((f) => JSON.parse(f.toString()))
  const buffers = frames.slice(delimIndex + 6)

  return {
    header,
    parent_header: parentHeader,
    metadata,
    content,
    buffers: buffers.length ? buffers.map((b) => b.toString('base64')) : undefined,
  }
}
