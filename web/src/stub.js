// MOCKED: a seeded fake fleet in the Watch snapshot shape, for the preview page and tests.

const SYSTEMS = [
  { weight: 60, os: 'linux', names: ['Ubuntu 24.04', 'Debian 13', 'Arch Linux', 'Fedora 42'], arch: 'x86_64', devices: ['AMD Radeon 780M', 'NVIDIA RTX 4090', 'Intel Arc A770', 'CPU'] },
  { weight: 20, os: 'macos', names: ['macOS 15.6', 'macOS 26.0'], arch: 'arm64', devices: ['Apple M4 Max', 'Apple M2 Pro', 'Apple M3'] },
  { weight: 20, os: 'freebsd', names: ['FreeBSD 16.0-CURRENT', 'FreeBSD 15.1-RELEASE'], arch: 'amd64', devices: ['CPU', 'AMD Radeon RX 7900'] },
]
const BACKEND = { CPU: 'cpu', 'Apple M4 Max': 'metal', 'Apple M2 Pro': 'metal', 'Apple M3': 'metal', 'NVIDIA RTX 4090': 'cuda' }
const MODELS = [
  { name: 'qwen3-8b', architecture: 'qwen3', class: 'generative', capabilities: ['reasoning'], parameters: 8190735360, quantization: 'Q4_K_M', trained_ctx: 40960 },
  { name: 'llama-3.1-8b', architecture: 'llama', class: 'generative', capabilities: ['tools'], parameters: 8030261248, quantization: 'Q4_K_M', trained_ctx: 131072 },
  { name: 'tinyllama', architecture: 'llama', class: 'generative', capabilities: [], parameters: 1100048384, quantization: 'Q4_K_M', trained_ctx: 2048 },
  { name: 'gemma-3-4b', architecture: 'gemma3', class: 'generative', capabilities: ['vision'], parameters: 3880263040, quantization: 'Q4_K_M', trained_ctx: 131072 },
  { name: 'minicheck', architecture: 'internlm2', class: 'decision', capabilities: [], parameters: 7737708544, quantization: 'Q4_K_M', trained_ctx: 32768 },
  { name: 'laya', architecture: 'modernbert', class: 'decision', capabilities: [], parameters: 149655232, quantization: 'F32', trained_ctx: 512 },
  { name: 'bge-m3', architecture: 'bert', class: 'embedding', capabilities: [], parameters: 567754752, quantization: 'F16', trained_ctx: 8192 },
  { name: 'qwen3-reranker', architecture: 'qwen3', class: 'reranker', capabilities: [], parameters: 595776512, quantization: 'Q8_0', trained_ctx: 40960 },
  { name: 'whisper-large', architecture: 'whisper', class: 'generative', capabilities: ['audio'], parameters: 1543490560, quantization: 'F16', trained_ctx: 448 },
]
/** The served context: the trained one, capped like MDMA_N_CTX=4096. */
const nCtx = (m) => Math.min(4096, m.trained_ctx)

/** mulberry32 */
function random(seed) {
  let a = seed >>> 0
  return () => {
    a = (a + 0x6d2b79f5) >>> 0
    let t = a
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

export function stubStatus({ peers = 100, seed = 1, federated = false } = {}) {
  const rand = random(seed)
  const pick = (list) => list[Math.floor(rand() * list.length)]
  const total = SYSTEMS.reduce((sum, s) => sum + s.weight, 0)
  const system = () => {
    let roll = rand() * total
    return SYSTEMS.find((s) => (roll -= s.weight) < 0) ?? SYSTEMS[0]
  }
  const hosts = Array.from({ length: peers }, (_, i) => {
    const s = system()
    const device = pick(s.devices)
    const gib = 2 ** 30
    const memory = pick([16, 32, 64, 128]) * gib
    const vram = device === 'CPU' ? 0 : pick([8, 16, 24, 48]) * gib
    const disk = pick([0.5, 1, 2, 4]) * 1024 * gib
    return {
      name: `node-${String(i + 1).padStart(3, '0')}`, os: s.os, os_name: pick(s.names), arch: s.arch, cpu: '', device,
      memory_bytes: memory, memory_available_bytes: Math.round(memory * rand()),
      device_memory_bytes: vram, device_available_bytes: Math.round(vram * rand()),
      disk_bytes: disk, disk_available_bytes: Math.round(disk * rand()),
    }
  })
  const residency = () => {
    const roll = rand()
    return roll < 0.5 ? 'resident' : roll < 0.85 ? 'available' : roll < 0.95 ? 'loading' : 'failed'
  }
  const leases = (state) => (state === 'resident' || state === 'loading') && rand() < 0.4 ? 1 + Math.floor(rand() * 3) : 0
  const modelsOf = () => {
    const count = 1 + Math.floor(rand() * 4)
    const chosen = new Set(Array.from({ length: count }, () => pick(MODELS)))
    return [...chosen]
  }
  const self = { name: 'periscope', os: 'macos', os_name: 'macOS 15.6', arch: 'arm64', cpu: 'Apple M4 Max', memory_bytes: 128 * 1024 ** 3, device: 'Apple M4 Max' }
  const local = [[MODELS[2], 'resident', 1], [MODELS[4], 'available', 0], [MODELS[0], 'loading', 1]]
    .map(([m, residency, leases]) => ({ ...m, kind: 'gguf', backend: 'metal', n_ctx: nCtx(m), ready: true, host: null, residency, leases, weight_bytes: 2 ** 30 }))
  const status = { connected: true, ready: true, state: 'serving', backend: 'metal', default_model: 'tinyllama', host: self, models: local, memory: null, federation: null }

  if (federated) {
    const fleet = hosts.map((host) => ({
      peer_id: `${host.name}.mdma.test`,
      endpoint: `https://${host.name}.mdma.test:7443`,
      state: rand() < 0.05 ? 'suspended' : 'active',
      host,
      catalog: rand() < 0.03 ? null : { server_state: 'serving', models: modelsOf().map((m) => ({ model: m.name, architecture: m.architecture, backend: BACKEND[host.device] ?? 'rocm', ready: true, resident: rand() < 0.5, class: m.class, capabilities: m.capabilities, parameters: m.parameters, quantization: m.quantization, trained_ctx: m.trained_ctx })) },
    }))
    status.federation = { authorized: true, enabled: true, server_id: 'periscope.mdma.test', trust_generation: 42, peers: fleet, routes: [] }
    return { status }
  }
  for (const host of hosts) {
    const ready = rand() >= 0.05
    for (const m of modelsOf()) {
      const state = residency()
      status.models.push({ ...m, name: `${host.name}-${m.name}`, kind: 'gguf', backend: `federated/${BACKEND[host.device] ?? 'rocm'}`, n_ctx: nCtx(m), ready, host, residency: state, leases: leases(state) })
    }
  }
  return { status }
}
