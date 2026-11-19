#!/usr/bin/env node
/**
 * Miyo 发版 / 打包助手（零依赖，Node >= 20）。
 *
 * 版本号单一事实源：frontend/src-tauri/tauri.conf.json（updater 以它为当前客户端版本），
 * 同步到 frontend/package.json（+ package-lock）与 frontend/src-tauri/Cargo.toml。
 *
 *   node scripts/release.mjs release <X.Y.Z|patch|minor|major> [--skip-checks] [--publish]
 *       升版本 → lint/test/build → 提交推送 → 打 tag 推送，触发 GitHub Actions 多平台构建；
 *       --publish 等待构建完成后自动把 Draft Release 转正（需本机已登录 gh）
 *   node scripts/release.mjs build [X.Y.Z|patch|minor|major] [--no-offline]
 *       本地打包当前平台（frontend 下即 npm run build:release），带版本参数时先升版本；
 *       Windows 另出内嵌 WebView2 的 *-setup-offline.exe；产物在 target/release/bundle/
 *   node scripts/release.mjs bump <X.Y.Z|patch|minor|major>
 *       仅同步版本文件
 *
 * 签名私钥（build）：TAURI_SIGNING_PRIVATE_KEY 环境变量 > ~/.tauri/miyo-updater.key；
 * 都没有时跳过 updater 产物（安装包照常生成）。CI 打包仍走 scripts/package-tauri.sh。
 */

import { existsSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { execSync, spawnSync } from 'node:child_process'
import { homedir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const FRONTEND = path.join(ROOT, 'frontend')
const BUNDLE = path.join(ROOT, 'target', 'release', 'bundle')
const KEY_FILE = path.join(homedir(), '.tauri', 'miyo-updater.key')

/** 相对仓库根的三个版本文件路径。 */
export const VERSION_FILES = {
  conf: 'frontend/src-tauri/tauri.conf.json',
  pkg: 'frontend/package.json',
  cargo: 'frontend/src-tauri/Cargo.toml',
}

// ---------- 纯函数（可单测） ----------

/** semver 校验：X.Y.Z（宽松：每段 ≥0 的数字）。 */
export function isValidSemver(v) {
  return /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(v)
}

/**
 * 解析目标版本：显式 X.Y.Z 直接返回；patch/minor/major 基于当前版本递增。
 * 非法输入抛错。
 */
export function resolveTargetVersion(current, arg) {
  if (!arg) throw new Error('缺少目标版本参数（X.Y.Z 或 patch/minor/major）')
  if (isValidSemver(arg)) return arg
  const m = /^(\d+)\.(\d+)\.(\d+)$/.exec(current)
  if (!m) throw new Error(`当前版本非法：${current}`)
  let [maj, min, pat] = [Number(m[1]), Number(m[2]), Number(m[3])]
  switch (arg) {
    case 'patch':
      pat += 1
      break
    case 'minor':
      min += 1
      pat = 0
      break
    case 'major':
      maj += 1
      min = 0
      pat = 0
      break
    default:
      throw new Error(`无法识别的版本参数：${arg}（可用 X.Y.Z / patch / minor / major）`)
  }
  return `${maj}.${min}.${pat}`
}

/** 读 JSON 文件的 version 字段。 */
export function readJsonVersion(file) {
  return JSON.parse(readFileSync(file, 'utf8')).version
}

/**
 * 把 Cargo.toml 里「包自身」的 version 行替换为指定值。
 * 只匹配文件中第一条顶层 `version = "…"`（依赖版本带缩进或形如 name = { version = … }，不受影响）。
 */
export function applyCargoVersion(content, version) {
  let replaced = false
  const out = content.split('\n').map((line) => {
    if (!replaced && /^version\s*=\s*"[^"]*"\s*$/.test(line)) {
      replaced = true
      return `version = "${version}"`
    }
    return line
  })
  if (!replaced) throw new Error('Cargo.toml 中未找到包自身 version 行')
  return out.join('\n')
}

/**
 * 同步全部版本文件到 target；返回实际写入的文件列表（相对路径）。
 * lockfile 由调用方执行 `npm install --package-lock-only` 处理。
 */
export function syncVersions(root, target) {
  if (!isValidSemver(target)) throw new Error(`非法目标版本：${target}`)

  const confPath = path.join(root, VERSION_FILES.conf)
  const pkgPath = path.join(root, VERSION_FILES.pkg)
  const cargoPath = path.join(root, VERSION_FILES.cargo)

  const conf = JSON.parse(readFileSync(confPath, 'utf8'))
  conf.version = target
  writeFileSync(confPath, JSON.stringify(conf, null, 2) + '\n')

  const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'))
  pkg.version = target
  writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + '\n')

  writeFileSync(cargoPath, applyCargoVersion(readFileSync(cargoPath, 'utf8'), target))

  return Object.values(VERSION_FILES)
}

/** 一致性检查：返回各处版本与不一致项列表。 */
export function checkConsistency(root) {
  const versions = {
    'tauri.conf.json': readJsonVersion(path.join(root, VERSION_FILES.conf)),
    'package.json': readJsonVersion(path.join(root, VERSION_FILES.pkg)),
    'Cargo.toml': (() => {
      const m = /^version\s*=\s*"([^"]+)"/m.exec(
        readFileSync(path.join(root, VERSION_FILES.cargo), 'utf8'),
      )
      if (!m) throw new Error('Cargo.toml 缺少包版本行')
      return m[1]
    })(),
  }
  const values = Object.values(versions)
  const ok = values.every((v) => v === values[0])
  return { ok, base: values[0], versions }
}

// ---------- CLI ----------

function sh(cmd, cwd = ROOT, env = process.env) {
  execSync(cmd, { stdio: 'inherit', cwd, env })
}

function gitOut(args) {
  return execSync(`git ${args}`, { cwd: ROOT, encoding: 'utf8' }).trim()
}

function sleep(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms)
}

function bump(arg) {
  const current = readJsonVersion(path.join(ROOT, VERSION_FILES.conf))
  const target = resolveTargetVersion(current, arg)
  syncVersions(ROOT, target)
  sh('npm install --package-lock-only', FRONTEND)
  console.log(`✓ 版本已同步：${current} → ${target}`)
  return target
}

/** 一条命令发版：bump → checks → commit/push → tag/push（→ 可选 publish）。 */
function releaseFlow(arg, flags) {
  const dirty = gitOut('status --porcelain')
  if (dirty) throw new Error(`工作区有未提交改动，请先处理干净再发版：\n${dirty}`)
  const branch = gitOut('rev-parse --abbrev-ref HEAD')

  const tag = `v${bump(arg)}`

  // 检查失败则保留版本改动供修复，修好后重跑本命令
  if (flags.has('--skip-checks')) {
    console.log('跳过本地检查（--skip-checks）')
  } else {
    for (const name of ['lint', 'test', 'build']) {
      console.log(`\n▶ 检查 ${name}…`)
      sh(`npm run ${name}`, FRONTEND)
    }
  }

  sh('git add -A')
  sh(`git commit -m "chore(release): ${tag}"`)
  sh('git push')
  sh(`git tag -a ${tag} -m "Miyo ${tag}"`)
  sh(`git push origin ${tag}`)
  console.log(`\n✓ 已推送 ${branch} 与标签 ${tag}，GitHub Actions 开始构建`)

  if (flags.has('--publish')) watchAndPublish(tag)
  else console.log('构建完成后到 GitHub Release 页将 Draft 点 Publish（或加 --publish 自动完成）')
}

/** 等待 tag 触发的 Release 构建结束并把 Draft 转正（需 gh CLI 已登录），最长约 40 分钟。 */
function watchAndPublish(tag) {
  if (spawnSync('gh', ['--version'], { stdio: 'ignore' }).status !== 0) {
    console.log('\nℹ 未检测到 gh CLI：构建完成后请到 GitHub Release 页手动点 Publish Draft')
    return
  }
  console.log(`\n⏳ 等待 ${tag} 的多平台构建出现（最长 40 分钟）…`)
  const deadline = Date.now() + 40 * 60 * 1000
  let runId = null
  while (!runId && Date.now() < deadline) {
    try {
      const out = execSync('gh run list --workflow=release.yml --limit 10 --json databaseId,headBranch,event', {
        cwd: ROOT,
        encoding: 'utf8',
      })
      runId = JSON.parse(out).find((r) => r.headBranch === tag && r.event === 'push')?.databaseId ?? null
    } catch {
      /* gh 偶发网络错误：忽略本轮 */
    }
    if (!runId) sleep(30_000)
  }
  if (!runId) {
    console.log('ℹ 未匹配到该 tag 的运行记录，稍后请手动检查并发布 Draft')
    return
  }
  // watch 失败（非 0）会抛错，草稿保留供排查
  sh(`gh run watch ${runId} --exit-status`)
  sh(`gh release edit ${tag} --draft=false`)
  console.log(`\n🎉 ${tag} 已正式发布，updater 将在客户端轮询时收到更新`)
}

/** 本地打包当前平台；Windows 额外产出 WebView2 离线安装包。 */
function buildFlow(arg, flags) {
  if (arg) bump(arg)
  const { ok, base: version, versions } = checkConsistency(ROOT)
  if (!ok) throw new Error(`版本不一致 ${JSON.stringify(versions)}，先执行 bump 统一`)

  let key = process.env.TAURI_SIGNING_PRIVATE_KEY?.trim()
  if (!key && existsSync(KEY_FILE)) key = readFileSync(KEY_FILE, 'utf8').trim()
  if (!key) console.warn(`⚠ 未找到签名私钥（TAURI_SIGNING_PRIVATE_KEY 或 ${KEY_FILE}），本次跳过 updater 产物`)
  const env = key
    ? { ...process.env, TAURI_SIGNING_PRIVATE_KEY: key, TAURI_SIGNING_PRIVATE_KEY_PASSWORD: process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ?? '' }
    : process.env

  if (!existsSync(path.join(FRONTEND, 'node_modules'))) sh('npm ci', FRONTEND)

  // --config 走文件而非内联 JSON：cmd 与 bash 的引号规则不同
  const overrideDir = path.join(ROOT, 'target', 'build-package')
  mkdirSync(overrideDir, { recursive: true })
  const tauriBuild = (label, bundle, extra = '') => {
    if (!key) bundle = { ...bundle, createUpdaterArtifacts: false }
    let cmd = `npm run tauri -- build ${extra}`
    if (Object.keys(bundle).length) {
      const file = path.join(overrideDir, `${label}.json`)
      writeFileSync(file, JSON.stringify({ bundle }, null, 2))
      cmd += ` --config "${file}"`
    }
    console.log(`\n> tauri build（${label}）`)
    sh(cmd, FRONTEND, env)
  }

  tauriBuild('default', {})

  if (process.platform === 'win32' && !flags.has('--no-offline')) {
    const nsisDir = path.join(BUNDLE, 'nsis')
    const exeName = readdirSync(nsisDir).find((f) => f.includes(`_${version}_`) && f.endsWith('-setup.exe'))
    if (!exeName) throw new Error(`未在 ${nsisDir} 找到 ${version} 的 NSIS 安装包`)
    const exePath = path.join(nsisDir, exeName)
    const offlinePath = path.join(nsisDir, exeName.replace(/-setup\.exe$/, '-setup-offline.exe'))
    // 离线包与默认包同名：先挪开默认包及签名，构建后无论成败都还原
    const moved = [exePath, `${exePath}.sig`].filter((p) => existsSync(p))
    for (const p of moved) renameSync(p, `${p}.online-bak`)
    try {
      tauriBuild('offline', { createUpdaterArtifacts: false, windows: { webviewInstallMode: { type: 'offlineInstaller' } } }, '--bundles nsis')
      rmSync(offlinePath, { force: true })
      renameSync(exePath, offlinePath)
    } finally {
      for (const p of moved) {
        rmSync(p, { force: true })
        renameSync(`${p}.online-bak`, p)
      }
    }
  }

  console.log(`\n✓ Miyo v${version} 打包完成${key ? '' : '（未签名，无 updater 产物）'}：`)
  for (const dir of existsSync(BUNDLE) ? readdirSync(BUNDLE) : []) {
    const full = path.join(BUNDLE, dir)
    if (!statSync(full).isDirectory()) continue
    for (const f of readdirSync(full)) {
      const p = path.join(full, f)
      if (f.includes(version) && statSync(p).isFile()) {
        console.log(`  ${path.relative(ROOT, p).padEnd(60)} ${(statSync(p).size / 1048576).toFixed(1).padStart(7)} MB`)
      }
    }
  }
}

const COMMANDS = {
  release: { flags: ['--skip-checks', '--publish'], run: releaseFlow },
  build: { flags: ['--no-offline'], run: buildFlow },
  bump: { flags: [], run: (arg) => bump(arg) },
}

const USAGE = [
  '用法：node scripts/release.mjs <命令>',
  '',
  '  release <X.Y.Z|patch|minor|major> [--skip-checks] [--publish]',
  '       升版本 → 本地检查 → 提交推送 → 打标签触发 CI 多平台构建',
  '  build [X.Y.Z|patch|minor|major] [--no-offline]',
  '       本地打包当前平台（可选先升版本）；Windows 另出 WebView2 离线包',
  '  bump <X.Y.Z|patch|minor|major>',
  '       仅同步全部版本文件（含 package-lock）',
].join('\n')

function main() {
  const [cmd, ...rest] = process.argv.slice(2)
  const command = COMMANDS[cmd]
  const flags = new Set(rest.filter((a) => a.startsWith('--')))
  const unknown = command ? [...flags].filter((f) => !command.flags.includes(f)) : []
  if (!command || unknown.length) {
    if (unknown.length) console.error(`✗ 未知参数：${unknown.join(' ')}\n`)
    console.log(USAGE)
    if (cmd !== undefined) process.exitCode = 1
    return
  }
  try {
    command.run(rest.find((a) => !a.startsWith('--')), flags)
  } catch (e) {
    // 子进程失败时其输出已直接打到终端，只补退出码
    if (e.status == null) console.error(`✗ ${e.message}`)
    process.exitCode = e.status || 1
  }
}

// 被 import（单测）时不执行 CLI
if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  main()
}
