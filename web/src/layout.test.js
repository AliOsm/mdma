import { describe, expect, test } from 'bun:test'
import { COLUMN_HEIGHT, PEER_SPACING, RING_LIMIT, bytes, residency, residencyNote, cellPosition, cellState, contextRing, federationLayout, hostLine, isUp, layerBands, memoryLayout, osRole, peerShell, resources, share, stateRole } from './layout.js'
import { stubStatus } from './stub.js'

const MiB = 1024 ** 2
const model = (name, extra = {}) => ({ name, kind: 'gguf', backend: 'cpu', architecture: 'llama', n_ctx: 4096, ready: true, weight_bytes: 100 * MiB, residency: 'resident', ...extra })

describe('memoryLayout', () => {
  test('stacks weights per device and scales the tallest column to the full height', () => {
    const { columns, satellites } = memoryLayout({ status: { backend: 'cpu', models: [model('a'), model('b', { weight_bytes: 300 * MiB })] } })
    expect(columns).toHaveLength(1)
    const [a, b] = columns[0].segments
    expect(a.y).toBe(0)
    expect(b.y).toBeGreaterThan(a.height)
    expect(b.height).toBeCloseTo(a.height * 3)
    expect(columns[0].height).toBeCloseTo(COLUMN_HEIGHT + 0.04 * 2, 5)
    expect(satellites).toHaveLength(0)
  })

  test('remote models orbit as satellites, not device segments', () => {
    const { columns, satellites } = memoryLayout({ status: { models: [model('local'), model('far', { backend: 'federated/rocm', host: { name: 'w' }, weight_bytes: 0 }), model('peer', { backend: 'federated/cpu' })] } })
    expect(columns[0].segments.map((s) => s.label)).toEqual(['local'])
    expect(satellites.map((s) => s.name)).toEqual(['far', 'peer'])
  })

  test('aliases of one upload share a block once the server reports uploads', () => {
    const memory = { kv_bytes: 50 * MiB, workspace_bytes: 10 * MiB, limit_bytes: 1024 * MiB, weights: [{ upload_id: 0, bytes: 100 * MiB, models: ['a', 'a2'] }], device: { name: 'gfx1103' } }
    const { columns } = memoryLayout({ status: { models: [model('a'), model('a2')], memory } })
    expect(columns[0].device).toBe('gfx1103')
    expect(columns[0].segments.map((s) => [s.kind, s.label])).toEqual([['weights', 'a = a2'], ['kv', 'KV cache'], ['workspace', 'workspace']])
    expect(memoryLayout({ status: { models: [model('g', { geometry: { n_layers: 22 } })] } }).columns[0].segments[0].layers).toBe(22)
    expect(columns[0].used).toBe(160 * MiB)
    expect(columns[0].capacity).toBe(1024 * MiB)
  })

  test('only loaded models take memory; the rest are listed by residency', () => {
    const models = [model('hot'), model('cold', { residency: 'available' }), model('warming', { residency: 'loading', leases: 1 }), model('bad', { residency: 'failed' })]
    const { columns } = memoryLayout({ status: { models } })
    expect(columns[0].segments.map((s) => s.label)).toEqual(['hot'])
    expect(columns[0].onDisk).toEqual([{ name: 'cold', note: 'on disk' }, { name: 'warming', note: 'loading · 1 lease' }, { name: 'bad', note: 'failed' }])
  })

  test('tiny segments stay visible and empty ones take no space', () => {
    const { columns } = memoryLayout({ status: { models: [model('big', { weight_bytes: 1024 ** 4 }), model('tiny', { weight_bytes: 1 })] } })
    expect(columns[0].segments[1].height).toBeGreaterThan(0)
    const empty = memoryLayout({ status: { models: [], memory: { kv_bytes: 0, workspace_bytes: 0, weights: [], device: { name: 'cpu' } } } })
    expect(empty.columns[0].segments.every((s) => s.height === 0)).toBe(true)
  })
})

describe('federationLayout', () => {
  const names = (atoms) => atoms.map((a) => a.name)
  const host = (name, os, os_name, arch, device) => ({ name, os, os_name, arch, device })

  test('without operator topology, peers are the routed targets', () => {
    const layout = federationLayout({ status: { state: 'serving', models: [model('x', { backend: 'federated/cpu', host: { name: 'w' } }), model('y', { backend: 'federated/cpu', host: { name: 'w' }, ready: false }), model('z', { backend: 'federated/rocm' })] } })
    expect(layout.authorized).toBe(false)
    expect(layout.self).toMatchObject({ label: 'this server', state: 'serving' })
    expect(layout.peers.map((p) => [p.id, names(p.models), p.state])).toEqual([['rocm', ['z'], 'reachable'], ['w', ['x', 'y'], 'unreachable']])
    expect(layout.peers[1].chips).toHaveLength(2)
  })

  test('this server carries its local models', () => {
    const models = [model('tinyllama'), model('minicheck', { class: 'decision' }), model('far', { backend: 'federated/cpu', host: { name: 'w' } })]
    const { self } = federationLayout({ status: { host: host('periscope', 'macos', 'macOS 15.6', 'arm64', 'Apple M4 Max'), models } })
    expect(self.label).toBe('periscope')
    expect(hostLine(self.host)).toBe('macOS 15.6 · arm64 · Apple M4 Max')
    expect(self.chips.map((c) => [c.name, c.class])).toEqual([['tinyllama', 'generative'], ['minicheck', 'decision']])
    expect(self.chips.every((c) => c.y > 0)).toBe(true)
  })

  test('peers are named by host, carry its OS and sort by it', () => {
    const forge = host('forge', 'linux', 'Ubuntu 24.04', 'x86_64', 'CPU')
    const citadel = host('citadel', 'freebsd', 'FreeBSD 16.0-CURRENT', 'amd64', 'CPU')
    const models = [
      model('a', { backend: 'federated/cpu', host: forge, capabilities: ['reasoning'] }),
      model('b', { backend: 'federated/cpu', host: citadel, class: 'decision' }),
    ]
    const layout = federationLayout({ status: { models } })
    expect(layout.peers.map((p) => [p.label, hostLine(p.host), p.detail])).toEqual([['citadel', 'FreeBSD 16.0-CURRENT · amd64 · CPU', ''], ['forge', 'Ubuntu 24.04 · x86_64 · CPU', '']])
    expect(layout.peers[0].chips[0]).toMatchObject({ name: 'b', class: 'decision' })
    expect(layout.peers[1].chips[0].capabilities).toEqual(['reasoning'])
    expect(JSON.stringify(layout)).not.toContain('localhost')
  })

  test('an operator without federation configured still sees routed targets', () => {
    const models = [model('x', { backend: 'federated/rocm', host: { name: 'foundry' } })]
    const layout = federationLayout({ status: { models, federation: { authorized: true, enabled: false, peers: [], routes: [] } } })
    expect(layout.peers.map((p) => p.id)).toEqual(['foundry'])
  })

  test('operator topology carries trust state, host and catalog classes', () => {
    const topology = {
      authorized: true, enabled: true, server_id: 'home', trust_generation: 7,
      peers: [
        { peer_id: 'lab', endpoint: 'https://lab:7443', state: 'active', host: host('lab', 'freebsd', 'FreeBSD 16.0-CURRENT', 'amd64', 'CPU'), catalog: { models: [{ model: 'm', class: 'decision', capabilities: [] }] } },
        { peer_id: 'old', endpoint: 'https://old:7443', state: 'active', catalog: null },
      ],
      routes: [{ alias: 'big', peer_id: 'lab', model: 'm', operation: 'synthesize' }],
    }
    const layout = federationLayout({ status: { state: 'serving', models: [], federation: topology } })
    expect(layout.self).toMatchObject({ label: 'home', detail: 'serving · trust generation 7' })
    expect(layout.peers[0]).toMatchObject({ id: 'old', stale: true, models: [] })
    expect(layout.peers[1]).toMatchObject({ id: 'lab', state: 'active', stale: false, routes: ['big'] })
    expect(layout.peers[1].chips[0]).toMatchObject({ name: 'm', class: 'decision' })
  })
})

describe('fullerene federation', () => {
  const distance = (a, b) => Math.hypot(a.x - b.x, a.y - b.y, a.z - b.z)
  const minPairDistance = (points) => Math.min(...points.flatMap((a, i) => points.slice(i + 1).map((b) => distance(a, b))))

  test('up to eight peers sit on one even ring', () => {
    for (const n of [1, 3, 6, RING_LIMIT]) {
      const shell = peerShell(n)
      expect(shell.sphere).toBe(false)
      expect(shell.slots.every((s) => s.y === 0 && Math.abs(Math.hypot(s.x, s.z) - shell.radius) < 1e-9)).toBe(true)
      if (n > 1) expect(minPairDistance(shell.slots)).toBeGreaterThanOrEqual(PEER_SPACING - 1e-9)
    }
  })

  test('a hundred peers cover a sphere without crowding, banded by OS', () => {
    const layout = federationLayout(stubStatus({ peers: 100 }))
    expect(layout.sphere).toBe(true)
    expect(layout.peers).toHaveLength(100)
    expect(minPairDistance(layout.peers)).toBeGreaterThanOrEqual(PEER_SPACING * 0.9)
    for (const peer of layout.peers) {
      for (const chip of peer.chips) {
        const nearest = Math.min(...layout.peers.map((p) => distance(p, chip)))
        expect(distance(peer, chip)).toBeCloseTo(nearest, 9)
      }
    }
    const bands = new Map()
    for (const peer of layout.peers) {
      const band = bands.get(peer.host.os) ?? { top: -Infinity, bottom: Infinity }
      bands.set(peer.host.os, { top: Math.max(band.top, peer.y), bottom: Math.min(band.bottom, peer.y) })
    }
    const ordered = [...bands.values()]
    ordered.slice(1).forEach((band, i) => expect(band.top).toBeLessThan(ordered[i].bottom))
    expect(layout.bands.map((b) => b.label.split(' ×')[0])).toEqual(['FreeBSD', 'Linux', 'macOS'])
    expect(layout.bands.reduce((sum, b) => sum + Number(b.label.split('×')[1]), 0)).toBe(100)
  })

  test('the stub is seeded and covers both views', () => {
    expect(stubStatus({ peers: 20, seed: 3 })).toEqual(stubStatus({ peers: 20, seed: 3 }))
    expect(stubStatus({ peers: 20, seed: 3 })).not.toEqual(stubStatus({ peers: 20, seed: 4 }))
    const federated = federationLayout(stubStatus({ peers: 40, federated: true }))
    expect(federated.authorized).toBe(true)
    expect(federated.peers).toHaveLength(40)
    expect(federated.peers.every((p) => p.host?.os_name)).toBe(true)
    const classes = new Set(federated.peers.flatMap((p) => p.chips.map((c) => c.class)))
    expect([...classes].sort()).toEqual(['decision', 'embedding', 'generative', 'reranker'])
  })
})

describe('context ring', () => {
  test('caps cells and maps positions to prompt/generated/empty', () => {
    expect(contextRing(512)).toEqual({ cells: 512, perCell: 1, perRow: 128, rows: 4 })
    expect(contextRing(131072)).toEqual({ cells: 2048, perCell: 64, perRow: 128, rows: 16 })
    expect(contextRing(64)).toEqual({ cells: 64, perCell: 1, perRow: 64, rows: 1 })
    expect(contextRing(0).cells).toBe(1)
    const [first, half] = [cellPosition(0, 64), cellPosition(32, 64)]
    expect(first.x).toBeCloseTo(4)
    expect(half.x).toBeCloseTo(-4)
    expect(cellPosition(64, 64).y).toBeCloseTo(0.14)
    expect([0, 1, 2, 3].map((i) => cellState(i, 1, 2, 1))).toEqual(['prompt', 'prompt', 'generated', 'empty'])
    expect(cellState(1, 64, 10, 60)).toBe('generated')
  })
})

test('layer bands follow whole layers and thin out when crowded', () => {
  expect(layerBands(0, 5)).toBe(0)
  expect(layerBands(1, 5)).toBe(0)
  expect(layerBands(32, 5)).toBe(32)
  expect(layerBands(32, 0.4)).toBe(8)
  expect(layerBands(80, 1)).toBe(20)
})

test('state roles and byte labels', () => {
  expect(['serving', 'degraded', 'draining', 'revoked', 'weird'].map(stateRole)).toEqual(['good', 'warning', 'serious', 'critical', 'muted'])
  expect([{ os: 'freebsd' }, { os: 'linux' }, { os: 'macos' }, { os: 'plan9' }, null].map(osRole)).toEqual(['os-freebsd', 'os-linux', 'os-macos', 'muted', 'muted'])
  expect(['reachable', 'active', 'unreachable', 'suspended'].map(isUp)).toEqual([true, true, false, false])
  expect(bytes(512)).toBe('512 B')
  expect(bytes(1536 * MiB)).toBe('1.5 GiB')
})

test('resources show what is free on each machine', () => {
  const GiB = 2 ** 30
  expect(share(4.1 * GiB, 8 * GiB)).toBe('4.1/8.0 GiB')
  expect(share(41 * GiB, 62.5 * GiB)).toBe('41/63 GiB')
  expect(share(1.2 * 2 ** 40, 1.8 * 2 ** 40)).toBe('1.2/1.8 TiB')
  const host = { memory_bytes: 64 * GiB, memory_available_bytes: 40 * GiB, device_memory_bytes: 0, device_available_bytes: 0, disk_bytes: 2 * 2 ** 40, disk_available_bytes: 2 ** 40 }
  expect(resources(host)).toBe('RAM 40/64 GiB · disk 1.0/2.0 TiB free')
  expect(resources({ ...host, device_memory_bytes: 8 * GiB, device_available_bytes: 4 * GiB })).toContain('GPU 4.0/8.0 GiB')
  expect(resources(null)).toBe('')
  expect(resources({ memory_bytes: 0 })).toBe('')
})

describe('residency', () => {
  test('statuses say it, catalogs only say whether a model is loaded', () => {
    expect([{ residency: 'loading' }, { resident: true }, { resident: false }, {}].map(residency)).toEqual(['loading', 'resident', 'available', 'available'])
    expect(residencyNote({ residency: 'resident', leases: 2 })).toBe('2 leases')
    expect(residencyNote({ residency: 'resident' })).toBe('')
    expect(residencyNote({ resident: false, leases: 1 })).toBe('on disk · 1 lease')
  })

  test('model atoms carry it from routed statuses and peer catalogs', () => {
    const routed = federationLayout({ status: { models: [model('far', { backend: 'federated/cpu', host: { name: 'w' }, residency: 'available', leases: 1 })] } })
    expect(routed.peers[0].chips[0]).toMatchObject({ residency: 'available', note: 'on disk · 1 lease' })
    const topology = { authorized: true, enabled: true, server_id: 'home', trust_generation: 1, routes: [], peers: [{ peer_id: 'lab', state: 'active', catalog: { models: [{ model: 'big', resident: true }, { model: 'small', resident: false }] } }] }
    const catalog = federationLayout({ status: { models: [model('here', { residency: 'failed' })], federation: topology } })
    expect(catalog.peers[0].chips.map((c) => [c.name, c.residency])).toEqual([['big', 'resident'], ['small', 'available']])
    expect(catalog.self.chips[0]).toMatchObject({ residency: 'failed', note: 'failed' })
  })
})
