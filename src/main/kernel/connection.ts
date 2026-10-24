/**
 * Connection files: the five ports, the transport, and the HMAC key a kernel
 * needs in order to talk to us.
 */

import { createServer } from 'node:net'
import { randomBytes, randomUUID } from 'node:crypto'
import { mkdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

export interface ConnectionInfo {
  ip: string
  transport: 'tcp'
  signature_scheme: 'hmac-sha256'
  key: string
  shell_port: number
  iopub_port: number
  stdin_port: number
  control_port: number
  hb_port: number
  kernel_name: string
}

/**
 * Reserve a free port by binding to :0 and immediately releasing it. There is
 * an inherent race between release and the kernel's bind, but this is exactly
 * what jupyter_client does, and the window is microseconds wide.
 */
function reservePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer()
    server.unref()
    server.on('error', reject)
    server.listen(0, '127.0.0.1', () => {
      const address = server.address()
      if (typeof address === 'string' || address === null) {
        server.close()
        reject(new Error('Could not determine a free port'))
        return
      }
      const { port } = address
      server.close(() => resolve(port))
    })
  })
}

export async function createConnectionInfo(kernelName: string): Promise<ConnectionInfo> {
  // Sequentially, not in parallel: concurrent :0 binds can hand back the same
  // port before any of them are released.
  const shell = await reservePort()
  const iopub = await reservePort()
  const stdin = await reservePort()
  const control = await reservePort()
  const hb = await reservePort()

  return {
    ip: '127.0.0.1',
    transport: 'tcp',
    signature_scheme: 'hmac-sha256',
    key: randomBytes(32).toString('hex'),
    shell_port: shell,
    iopub_port: iopub,
    stdin_port: stdin,
    control_port: control,
    hb_port: hb,
    kernel_name: kernelName,
  }
}

export async function writeConnectionFile(info: ConnectionInfo): Promise<string> {
  const dir = join(tmpdir(), 'herse', 'connections')
  await mkdir(dir, { recursive: true })
  const path = join(dir, `kernel-${randomUUID()}.json`)
  // 0600: the file contains the HMAC key that authenticates messages.
  await writeFile(path, JSON.stringify(info, null, 2), { mode: 0o600 })
  return path
}

export async function removeConnectionFile(path: string): Promise<void> {
  await rm(path, { force: true }).catch(() => {})
}

export function channelUrl(info: ConnectionInfo, port: number): string {
  return `${info.transport}://${info.ip}:${port}`
}
