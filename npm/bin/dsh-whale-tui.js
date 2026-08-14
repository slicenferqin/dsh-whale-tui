#!/usr/bin/env node
// Standalone entry: spawn the native binary from vendor/<platform-arch>.
// Plugin mode (dsh --profile tui) is the recommended path; see README.
import { spawn } from 'node:child_process'
import path from 'node:path'

const key = process.platform + '-' + process.arch
const exe = process.platform === 'win32' ? 'dsh-whale-tui.exe' : 'dsh-whale-tui'
const bin = path.join(path.dirname(new URL(import.meta.url).pathname), '..', 'vendor', key, exe)

const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit' })
child.on('error', (err) => {
  console.error('dsh-whale-tui: native binary not found for ' + key + ' (' + bin + ')')
  console.error('build it with scripts/build-npm.sh')
  console.error(err.message)
  process.exit(1)
})
child.on('exit', (code) => process.exit(code === null ? 1 : code))
