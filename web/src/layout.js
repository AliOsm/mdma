// Data -> geometry for the three views. y is up.

export const COLUMN_HEIGHT = 6
const COLUMN_SPACING = 3.2
const MIN_SEGMENT = 0.08
const SEGMENT_GAP = 0.04

/** Layer bands drawn on a weight block: one per layer, thinned when crowded. */
export function layerBands(layers, height, minSpacing = 0.05) {
  if (!layers || layers < 2) return 0
  const fit = Math.floor(height / minSpacing)
  if (fit >= layers) return layers
  const every = Math.ceil(layers / Math.max(1, fit))
  return Math.max(1, Math.floor(layers / every))
}

/** Where a remote model runs: its node name, else its address. */
export const remoteTarget = (m) => m.host?.name || (m.backend ?? '').slice('federated/'.length)

export const isRemote = (m) => (m.backend ?? '').startsWith('federated/')

/** resident | available | loading | failed; catalogs only say whether a model is loaded. */
export const residency = (m) => m.residency || (m.resident ? 'resident' : 'available')

export const isLoaded = (m) => residency(m) === 'resident'

/** "on disk", "loading", "failed" and lease counts, for labels. */
export function residencyNote(m) {
  const leases = m.leases ? `${m.leases} lease${m.leases === 1 ? '' : 's'}` : ''
  const state = { available: 'on disk', loading: 'loading', failed: 'failed' }[residency(m)] ?? ''
  return [state, leases].filter(Boolean).join(' · ')
}

export function bytes(n) {
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB']
  let value = n
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return unit === 0 ? `${n} B` : `${value.toFixed(1)} ${units[unit]}`
}

/** Memory & placement: one column per device, loaded models stacked bottom-up; the rest listed on disk. */
export function memoryLayout(scene) {
  const status = scene?.status ?? { models: [] }
  const memory = status.memory ?? null
  const local = status.models.filter((m) => !isRemote(m))
  const remote = status.models.filter(isRemote)

  const byDevice = new Map()
  for (const m of local) {
    const device = memory?.device?.name || m.backend || status.backend || 'local'
    if (!byDevice.has(device)) byDevice.set(device, [])
    byDevice.get(device).push(m)
  }
  if (byDevice.size === 0 && memory) byDevice.set(memory.device?.name || status.backend || 'local', [])

  const columns = [...byDevice].map(([device, models]) => {
    const segments = weightSegments(models.filter(isLoaded), memory)
    const onDisk = models.filter((m) => !isLoaded(m)).map((m) => ({ name: m.name, note: residencyNote(m) }))
    if (memory) {
      for (const [kind, label] of [['kv', 'KV cache'], ['workspace', 'workspace']]) {
        segments.push({ kind, label, bytes: memory[`${kind}_bytes`] ?? 0 })
      }
    }
    const used = segments.reduce((sum, s) => sum + s.bytes, 0)
    // Budget limit, else used plus free device memory.
    const ceiling = memory?.limit_bytes || (memory?.device?.available_bytes ? memory.device.available_bytes + used : 0)
    const capacity = Math.max(used, ceiling)
    return { device, segments, onDisk, used, capacity }
  })

  const scale = COLUMN_HEIGHT / Math.max(1, ...columns.map((c) => c.capacity))
  columns.forEach((column, i) => {
    column.x = (i - (columns.length - 1) / 2) * COLUMN_SPACING
    let y = 0
    for (const segment of column.segments) {
      segment.height = segment.bytes > 0 ? Math.max(MIN_SEGMENT, segment.bytes * scale) : 0
      segment.y = y
      y += segment.height + (segment.height > 0 ? SEGMENT_GAP : 0)
    }
    column.top = y
    column.height = Math.max(y, column.capacity * scale)
  })

  const radius = Math.max(4.5, columns.length * COLUMN_SPACING * 0.75)
  const satellites = remote.map((m, i) => {
    const angle = (i / Math.max(1, remote.length)) * Math.PI * 2
    return {
      name: m.name,
      target: remoteTarget(m),
      ready: m.ready,
      residency: residency(m),
      x: Math.cos(angle) * radius,
      y: COLUMN_HEIGHT * 0.5,
      z: Math.sin(angle) * radius,
    }
  })
  return { columns, satellites }
}

/** Aliases of one upload share a block. */
function weightSegments(models, memory) {
  const uploads = new Map()
  for (const m of models) {
    const upload = memory?.weights?.find((w) => w.models?.includes(m.name))
    const key = upload ? `upload:${upload.upload_id}` : `model:${m.name}`
    const bytes = upload ? upload.bytes : m.weight_bytes
    if (!uploads.has(key)) uploads.set(key, { kind: 'weights', names: [], bytes, ready: true, layers: m.geometry?.n_layers ?? 0 })
    const entry = uploads.get(key)
    entry.names.push(m.name)
    entry.ready &&= m.ready
  }
  return [...uploads.values()].map((u) => ({ ...u, label: u.names.join(' = ') }))
}

/** Federation as a molecule: this server in the middle with its models, peers around it with theirs. */
export function federationLayout(scene) {
  const status = scene?.status ?? { models: [] }
  const topology = status.federation
  const federated = Boolean(topology?.authorized && topology.enabled)
  let peers
  if (federated) {
    peers = topology.peers.map((p) => ({
      id: p.peer_id,
      label: p.peer_id,
      host: p.host ?? null,
      detail: p.endpoint,
      state: p.state,
      models: (p.catalog?.models ?? []).map(atom),
      stale: p.state === 'active' && !p.catalog,
      routes: topology.routes.filter((r) => r.peer_id === p.peer_id).map((r) => r.alias),
    }))
  } else {
    const byTarget = new Map()
    for (const m of status.models.filter(isRemote)) {
      const target = remoteTarget(m)
      if (!byTarget.has(target)) {
        const backend = (m.backend ?? '').replace(/^federated\//, '')
        const device = m.host?.device?.toLowerCase() === backend ? '' : backend
        byTarget.set(target, { id: target, label: target, host: m.host ?? null, detail: device, models: [], stale: false, routes: [], ready: true })
      }
      const peer = byTarget.get(target)
      peer.models.push(atom(m))
      peer.ready &&= m.ready
    }
    peers = [...byTarget.values()].map(({ ready, ...p }) => ({ ...p, state: ready ? 'reachable' : 'unreachable' }))
  }
  peers.sort((a, b) => (a.host?.os ?? '').localeCompare(b.host?.os ?? '') || a.label.localeCompare(b.label))
  const shell = peerShell(peers.length)
  peers.forEach((peer, i) => {
    Object.assign(peer, shell.slots[i])
    peer.chips = bonded(peer, peer.models, CHIP_BOND)
  })
  const state = status.state ?? 'unknown'
  const host = status.host ?? null
  return {
    self: {
      label: topology?.server_id || host?.name || 'this server',
      host,
      detail: [status.backend, state, federated && `trust generation ${topology.trust_generation}`].filter(Boolean).join(' · '),
      state,
      chips: bonded({ x: 0, y: 0, z: 0, normal: UP }, status.models.filter((m) => !isRemote(m)).map(atom), SELF_BOND),
    },
    peers,
    sphere: shell.sphere,
    radius: shell.radius,
    bands: shell.sphere ? osBands(peers, shell.radius) : [],
    authorized: federated,
  }
}

/** What the scene needs of a model. */
const atom = (m) => ({
  name: m.name ?? m.model, class: m.class || 'generative', capabilities: m.capabilities ?? [], ready: m.ready !== false,
  residency: residency(m), note: residencyNote(m),
})

/** "FreeBSD 16.0-CURRENT · amd64 · CPU" */
export const hostLine = (host) => (host ? [host.os_name, host.arch, host.device].filter(Boolean).join(' · ') : '')

/** `free/total` in the unit of `total`: "4.1/8.0 GiB". */
export function share(free, total) {
  const [unit, scale] = total >= 2 ** 40 ? ['TiB', 2 ** 40] : ['GiB', 2 ** 30]
  const number = (bytes) => {
    const value = bytes / scale
    return value < 10 ? value.toFixed(1) : value.toFixed(0)
  }
  return `${number(free)}/${number(total)} ${unit}`
}

/** "RAM 41/62 GiB · GPU 4.1/8.0 GiB · disk 1.2/1.8 TiB free" */
export function resources(host) {
  if (!host) return ''
  const parts = [['RAM', host.memory_available_bytes, host.memory_bytes], ['GPU', host.device_available_bytes, host.device_memory_bytes], ['disk', host.disk_available_bytes, host.disk_bytes]]
    .filter(([, , total]) => total > 0)
    .map(([label, free, total]) => `${label} ${share(free ?? 0, total)}`)
  return parts.length ? `${parts.join(' · ')} free` : ''
}

export const PEER_SPACING = 3.4
export const RING_LIMIT = 8
export const CHIP_BOND = 1.5
const SELF_BOND = 1.9
const MIN_RADIUS = 5
const CONE = 0.6
const UP = { x: 0, y: 1, z: 0 }
const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5))

/**
 * Up to RING_LIMIT peers sit on an even ring; more cover a Fibonacci sphere
 * whose radius keeps neighbours about PEER_SPACING apart.
 */
export function peerShell(n) {
  if (n <= RING_LIMIT) {
    const radius = n < 2 ? MIN_RADIUS : Math.max(MIN_RADIUS, PEER_SPACING / (2 * Math.sin(Math.PI / n)))
    const slots = Array.from({ length: n }, (_, i) => {
      const a = (i / n) * Math.PI * 2
      return slot({ x: Math.cos(a), y: 0, z: Math.sin(a) }, radius)
    })
    return { sphere: false, radius, slots }
  }
  // Hexagonal packing: each point owns 4πR²/n = (√3/2)·d², padded for the poles.
  const radius = Math.max(MIN_RADIUS, 1.12 * PEER_SPACING * Math.sqrt((Math.sqrt(3) * n) / (8 * Math.PI)))
  const slots = Array.from({ length: n }, (_, i) => {
    const y = 1 - (2 * (i + 0.5)) / n
    const r = Math.sqrt(1 - y * y)
    const a = i * GOLDEN_ANGLE
    return slot({ x: Math.cos(a) * r, y, z: Math.sin(a) * r }, radius)
  })
  return { sphere: true, radius, slots }
}

const slot = (normal, radius) => ({ x: normal.x * radius, y: normal.y * radius, z: normal.z * radius, normal })

/** Models on a cone around the atom's outward normal, like hydrogens. */
function bonded(center, models, length) {
  const n = center.normal ?? normalize(center)
  const helper = Math.abs(n.y) < 0.9 ? UP : { x: 1, y: 0, z: 0 }
  const t1 = normalize(cross(n, helper))
  const t2 = cross(n, t1)
  return models.map((model, j) => {
    const spread = models.length === 1 ? 0 : CONE
    const phi = (j / models.length) * Math.PI * 2
    const [c, s] = [Math.cos(spread), Math.sin(spread)]
    const dir = {
      x: n.x * c + (t1.x * Math.cos(phi) + t2.x * Math.sin(phi)) * s,
      y: n.y * c + (t1.y * Math.cos(phi) + t2.y * Math.sin(phi)) * s,
      z: n.z * c + (t1.z * Math.cos(phi) + t2.z * Math.sin(phi)) * s,
    }
    return { ...model, x: center.x + dir.x * length, y: center.y + dir.y * length, z: center.z + dir.z * length }
  })
}

const OS_NAMES = { freebsd: 'FreeBSD', linux: 'Linux', macos: 'macOS', openbsd: 'OpenBSD', netbsd: 'NetBSD', windows: 'Windows' }

/** One label per OS family, just outside its band of the sphere. */
function osBands(peers, radius) {
  const groups = new Map()
  for (const peer of peers) {
    const os = peer.host?.os || ''
    if (!groups.has(os)) groups.set(os, [])
    groups.get(os).push(peer)
  }
  return [...groups].map(([os, members]) => {
    const sum = members.reduce((acc, p) => ({ x: acc.x + p.normal.x, y: acc.y + p.normal.y, z: acc.z + p.normal.z }), { x: 0, y: 0, z: 0 })
    const dir = Math.hypot(sum.x, sum.y, sum.z) > 1e-6 ? normalize(sum) : members[0].normal
    const at = radius + CHIP_BOND + 2
    return { os, label: `${OS_NAMES[os] ?? (os || 'unknown')} ×${members.length}`, x: dir.x * at, y: dir.y * at, z: dir.z * at }
  })
}

function normalize(v) {
  const length = Math.hypot(v.x, v.y, v.z) || 1
  return { x: v.x / length, y: v.y / length, z: v.z / length }
}

const cross = (a, b) => ({ x: a.y * b.z - a.z * b.y, y: a.z * b.x - a.x * b.z, z: a.x * b.y - a.y * b.x })

export const OS_FAMILIES = ['freebsd', 'linux', 'macos']

/** OS family -> palette role. */
export const osRole = (host) => (OS_FAMILIES.includes(host?.os) ? `os-${host.os}` : 'muted')

/** Peers that answer; others are drawn faded. */
export const isUp = (state) => stateRole(state) === 'good'

/** State -> palette role. */
export function stateRole(state) {
  switch (state) {
    case 'serving': case 'active': case 'reachable': return 'good'
    case 'degraded': case 'staged': return 'warning'
    case 'draining': case 'suspended': case 'connecting': return 'serious'
    case 'critical': case 'revoked': case 'unreachable': return 'critical'
    default: return 'muted'
  }
}

export const RING_CELLS = 2048
export const RING_PER_ROW = 128

/** The context window as a tube of cells; one cell covers `perCell` positions. */
export function contextRing(nCtx, maxCells = RING_CELLS) {
  const positions = Math.max(1, nCtx || 1)
  const perCell = Math.ceil(positions / maxCells)
  const cells = Math.ceil(positions / perCell)
  const perRow = Math.min(RING_PER_ROW, cells)
  return { cells, perCell, perRow, rows: Math.ceil(cells / perRow) }
}

export function cellPosition(index, perRow, radius = 4, rowHeight = 0.14) {
  const angle = ((index % perRow) / perRow) * Math.PI * 2
  return { x: Math.cos(angle) * radius, y: Math.floor(index / perRow) * rowHeight, z: Math.sin(angle) * radius, angle }
}

/** Which positions are filled: prompt first, then generated tokens. */
export function cellState(index, perCell, promptTokens, emitted) {
  const start = index * perCell
  if (start < promptTokens) return 'prompt'
  if (start < promptTokens + emitted) return 'generated'
  return 'empty'
}
