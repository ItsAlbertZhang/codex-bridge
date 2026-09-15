#!/usr/bin/env node
/**
 * One command: bump the patch version, build, pack, then reinstall the tarball
 * into the dsh profile.
 *
 *   node scripts/reinstall.mjs [--profile bridge] [--no-bump] [--dsh-bin <path>]
 *
 * Why `remove` before `add`: the profile lockfile records the previous tarball
 * by absolute path. Once that file is gone, the next `add` fails with ENOENT
 * while resolving the stale dependency. Removing first clears the entry.
 *
 * The dsh process does NOT hot-reload a changed bundle: the bundle set is
 * frozen at startup. Restart dsh after this script.
 */

import { execFileSync } from 'node:child_process'
import { accessSync, constants, readFileSync, writeFileSync, rmSync, existsSync, statSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { delimiter, dirname, join, resolve } from 'node:path'
import { argv, exit, platform } from 'node:process'

const root = dirname(dirname(fileURLToPath(import.meta.url)))
const manifestPath = join(root, 'package.json')

function flag(name, fallback) {
  const index = argv.indexOf(name)
  return index >= 0 && index + 1 < argv.length ? argv[index + 1] : fallback
}

const profile = flag('--profile', 'bridge')
const dshBin = flag('--dsh-bin', undefined) || process.env.AGENT_BRIDGE_DSH_BIN || undefined
const bump = !argv.includes('--no-bump')

function dshOnPath() {
  const names = platform === 'win32' ? ['dsh.exe', 'dsh.com', 'dsh.cmd', 'dsh.bat'] : ['dsh']
  for (const directory of (process.env.PATH ?? '').split(delimiter).filter(Boolean)) {
    for (const name of names) {
      const candidate = resolve(directory, name)
      try {
        if (!statSync(candidate).isFile()) continue
        accessSync(candidate, platform === 'win32' ? constants.F_OK : constants.X_OK)
        return candidate
      } catch {
        // Continue searching when a PATH entry is missing or inaccessible.
      }
    }
  }
  return undefined
}

const dshExecutable = dshBin === undefined ? dshOnPath() : process.execPath
if (dshExecutable === undefined) {
  console.error('dsh was not found on PATH; set AGENT_BRIDGE_DSH_BIN to the dsh JavaScript entry point or pass --dsh-bin <path>')
  exit(1)
}

function run(file, args, options = {}) {
  console.log(`> ${file} ${args.join(' ')}`)
  execFileSync(file, args, { cwd: root, stdio: 'inherit', shell: platform === 'win32', ...options })
}

function runDsh(args) {
  if (dshBin !== undefined) args = [dshBin, ...args]
  if (platform !== 'win32' || !/\.(?:cmd|bat)$/i.test(dshExecutable)) {
    run(dshExecutable, args, { shell: false })
    return
  }
  // Batch shims require cmd; escape each shell parsing pass, including the shim's.
  const escapeMeta = value => value.replace(/([()[\]%!^"`<>&|;, *?])/g, '^$1')
  const escapeArgument = value => escapeMeta(escapeMeta(`"${value.replace(/(\\*)"/g, '$1$1\\"').replace(/(\\*)$/, '$1$1')}"`))
  const command = [escapeMeta(dshExecutable), ...args.map(escapeArgument)].join(' ')
  run(process.env.ComSpec || 'cmd.exe', ['/d', '/s', '/v:off', '/c', `"${command}"`], {
    shell: false,
    windowsVerbatimArguments: true,
    windowsHide: true,
  })
}

const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'))
const previousTarball = join(root, `${manifest.name}-${manifest.version}.tgz`)

if (bump) {
  const parts = manifest.version.split('.')
  parts[2] = String(Number(parts[2]) + 1)
  manifest.version = parts.join('.')
  writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`)
  console.log(`version -> ${manifest.version}`)
}

run('npx', ['tsc', '-p', 'tsconfig.json'])
run('npx', ['tsdown'])
run('pnpm', ['pack'])

const tarball = join(root, `${manifest.name}-${manifest.version}.tgz`)
if (!existsSync(tarball)) {
  console.error(`pack did not produce ${tarball}`)
  exit(1)
}

// remove must finish while the previous tarball is still on disk.
try {
  runDsh(['plugin', '--profile', profile, 'remove', manifest.name])
} catch {
  console.log('(remove failed or the plugin was not installed; continuing)')
}
runDsh(['plugin', '--profile', profile, 'add', tarball])

if (previousTarball !== tarball && existsSync(previousTarball)) rmSync(previousTarball)

console.log(`\ninstalled ${manifest.name}@${manifest.version} into profile "${profile}"`)
console.log('restart dsh for the new bundle to take effect')
