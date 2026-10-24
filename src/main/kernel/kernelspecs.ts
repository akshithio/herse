/**
 * Kernelspec discovery.
 *
 * We ask `jupyter kernelspec list --json` first because it knows about
 * environment-specific paths we can't guess, then merge in a scan of the
 * standard directories so the app still works with no `jupyter` CLI on PATH
 * (a bare `ipykernel` install, a conda env, etc.).
 */

import { execFile } from 'node:child_process'
import { homedir, platform } from 'node:os'
import { join } from 'node:path'
import { promisify } from 'node:util'
import { readFile, readdir, stat } from 'node:fs/promises'
import type { KernelSpec } from '@shared/jupyter'

const execFileAsync = promisify(execFile)

function systemKernelDirs(): string[] {
  const home = homedir()
  const dirs: string[] = []

  if (platform() === 'darwin') {
    dirs.push(join(home, 'Library', 'Jupyter', 'kernels'))
    dirs.push('/usr/local/share/jupyter/kernels', '/usr/share/jupyter/kernels')
  } else if (platform() === 'win32') {
    const appData = process.env.APPDATA
    const programData = process.env.PROGRAMDATA
    if (appData) dirs.push(join(appData, 'jupyter', 'kernels'))
    if (programData) dirs.push(join(programData, 'jupyter', 'kernels'))
  } else {
    dirs.push(join(home, '.local', 'share', 'jupyter', 'kernels'))
    dirs.push('/usr/local/share/jupyter/kernels', '/usr/share/jupyter/kernels')
  }

  // Legacy location, still used by some installs on every platform.
  dirs.push(join(home, '.ipython', 'kernels'))
  return dirs
}

function normalize(name: string, raw: Record<string, unknown>, resourceDir?: string): KernelSpec | null {
  const argv = raw.argv
  if (!Array.isArray(argv) || argv.length === 0) return null
  return {
    name,
    display_name: typeof raw.display_name === 'string' ? raw.display_name : name,
    language: typeof raw.language === 'string' ? raw.language : 'unknown',
    argv: argv.map(String),
    env: (raw.env as Record<string, string>) ?? undefined,
    interrupt_mode: raw.interrupt_mode === 'message' ? 'message' : 'signal',
    metadata: (raw.metadata as Record<string, unknown>) ?? undefined,
    resource_dir: resourceDir,
  }
}

async function fromJupyterCli(): Promise<KernelSpec[]> {
  try {
    const { stdout } = await execFileAsync('jupyter', ['kernelspec', 'list', '--json'], {
      timeout: 8000,
      maxBuffer: 4 * 1024 * 1024,
    })
    const parsed = JSON.parse(stdout) as {
      kernelspecs?: Record<string, { spec?: Record<string, unknown>; resource_dir?: string }>
    }
    const specs: KernelSpec[] = []
    for (const [name, entry] of Object.entries(parsed.kernelspecs ?? {})) {
      if (!entry.spec) continue
      const spec = normalize(name, entry.spec, entry.resource_dir)
      if (spec) specs.push(spec)
    }
    return specs
  } catch {
    // No `jupyter` on PATH, or it failed — the directory scan below covers us.
    return []
  }
}

async function fromDirectories(): Promise<KernelSpec[]> {
  const specs: KernelSpec[] = []

  for (const dir of systemKernelDirs()) {
    let entries: string[]
    try {
      entries = await readdir(dir)
    } catch {
      continue
    }

    for (const name of entries) {
      const resourceDir = join(dir, name)
      try {
        if (!(await stat(resourceDir)).isDirectory()) continue
        const raw = JSON.parse(await readFile(join(resourceDir, 'kernel.json'), 'utf8'))
        const spec = normalize(name, raw, resourceDir)
        if (spec) specs.push(spec)
      } catch {
        // Not a kernelspec directory; skip it.
      }
    }
  }

  return specs
}

/**
 * Discovered specs, deduplicated by name. The Jupyter CLI wins on conflict
 * because its view accounts for the active environment.
 */
export async function discoverKernelSpecs(): Promise<KernelSpec[]> {
  const [cli, scanned] = await Promise.all([fromJupyterCli(), fromDirectories()])

  const byName = new Map<string, KernelSpec>()
  for (const spec of scanned) byName.set(spec.name, spec)
  for (const spec of cli) byName.set(spec.name, spec)

  return [...byName.values()].sort((a, b) => a.display_name.localeCompare(b.display_name))
}

export async function findKernelSpec(name: string): Promise<KernelSpec | undefined> {
  return (await discoverKernelSpecs()).find((spec) => spec.name === name)
}
