// The three views.
import * as THREE from 'three'
import { CHIP_BOND, bytes, cellPosition, cellState, contextRing, federationLayout, hostLine, isUp, layerBands, memoryLayout, osRole, reservedKv, resources, stateRole } from './layout.js'

const SEGMENT_ROLE = { weights: 'series-1', kv: 'series-2', workspace: 'series-3' }
/** How a model atom shows its residency. */
const LOOK = { resident: { opacity: 1 }, available: { opacity: 0.3 }, loading: { opacity: 0.65, pulse: true }, failed: { opacity: 0.3, ring: 'critical' } }

export class MemoryView {
  constructor(stage) {
    this.stage = stage
    stage.onTheme = () => this.data && this.scene(this.data)
  }

  scene(data) {
    this.data = data
    const { stage } = this
    stage.clear()
    const { columns, satellites } = memoryLayout(data)
    for (const column of columns) {
      const base = [column.x, 0, 0]
      stage.box('grid', [2.4, 0.06, 2.4], [column.x, -0.06, 0])
      stage.label([column.device, `${bytes(column.used)} of ${bytes(column.capacity)}`], [column.x, -0.5, 1.4], 'device')
      if (column.height > column.top + 0.01) stage.outline([1.9, column.height, 1.9], base)
      for (const segment of column.segments.filter((s) => s.height > 0)) {
        const mesh = stage.box(SEGMENT_ROLE[segment.kind], [1.6, segment.height, 1.6], [column.x, segment.y, 0], { opacity: segment.ready === false ? 0.35 : 1 })
        mesh.userData = segment
        const bands = segment.kind === 'weights' ? layerBands(segment.layers, segment.height) : 0
        if (bands > 1) stage.bands(bands, [1.6, segment.height, 1.6], [column.x, segment.y, 0])
        const detail = segment.kind === 'weights' ? [segment.facts, bytes(segment.bytes), segment.layers && `${segment.layers} layers`].filter(Boolean).join(' · ') : bytes(segment.bytes)
        stage.label([segment.label, detail], [column.x + 0.95, segment.y + segment.height / 2, 0.8], `segment ${segment.kind}`, [0, 0.5])
      }
      if (column.onDisk.length) {
        stage.label(['not loaded', ...column.onDisk.map((m) => `${m.name} · ${m.note}`)], [column.x, -1.1, 1.4], 'idle', [0.5, 0])
      }
    }
    for (const sat of satellites) {
      stage.sphere(sat.ready ? 'good' : 'critical', 0.3, [sat.x, sat.y, sat.z], { opacity: sat.residency === 'resident' ? 1 : 0.4 })
      stage.line([0, columns[0]?.top ?? 0, 0], [sat.x, sat.y, sat.z])
      stage.label([sat.name, sat.ready ? `→ ${sat.target}` : `→ ${sat.target} (unavailable)`], [sat.x, sat.y + 0.55, sat.z], 'remote')
    }
    if (columns.length === 0 && satellites.length === 0) stage.label(['No models reported yet'], [0, 1, 0], 'empty')
  }
}

export class FederationView {
  constructor(stage) {
    this.stage = stage
    this.pulsing = []
    stage.controls.target.set(0, 0, 0)
    stage.onTheme = () => {
      this.key = null
      if (this.data) this.scene(this.data)
    }
    // Loading models breathe.
    stage.onFrame = (time, motion) => {
      const scale = motion ? 1 + 0.2 * Math.sin(time / 220) : 1
      for (const mesh of this.pulsing) mesh.scale.setScalar(scale)
    }
  }

  /** Ball and stick: this server and its models in the middle, peers on a ring or a fullerene shell. */
  scene(data) {
    this.data = data
    const layout = federationLayout(data)
    const key = JSON.stringify(layout)
    if (key === this.key) return
    this.key = key
    const { stage } = this
    stage.clear()
    this.pulsing = []
    this.frame(layout)
    const crowded = layout.sphere
    const near = { chip: crowded ? 11 : 30, peer: crowded ? 16 : Infinity, name: crowded ? 24 : Infinity }
    stage.sphere(osRole(layout.self.host), 0.8, [0, 0, 0])
    stage.label([layout.self.label, hostLine(layout.self.host), resources(layout.self.host), layout.self.detail].filter(Boolean), [0, -1.25, 0], 'self')
    this.chips({ x: 0, y: 0, z: 0 }, layout.self.chips, 'self', near.chip)
    for (const peer of layout.peers) {
      const role = stateRole(peer.state)
      const at = [peer.x, peer.y, peer.z]
      stage.bond([0, 0, 0], at, role, crowded ? 0.03 : 0.09)
      // Atoms by OS, spokes by state.
      stage.pick(stage.sphere(osRole(peer.host), 0.5, at, { opacity: peer.stale || !isUp(peer.state) ? 0.35 : 1 }), peer.id)
      const routes = peer.routes.length ? `routes: ${peer.routes.join(', ')}` : ''
      stage.label([peer.label], [peer.x, peer.y + 0.6, peer.z], 'peer', [0.5, 1], { near: near.name, group: peer.id })
      stage.label([hostLine(peer.host), resources(peer.host), [peer.state, peer.stale && 'no fresh catalog', peer.detail].filter(Boolean).join(' · '), routes].filter(Boolean),
        [peer.x, peer.y - 0.6, peer.z], 'peer detail', [0.5, 0], { near: near.peer, group: peer.id })
      this.chips(peer, peer.chips, peer.id, near.chip)
    }
    for (const band of layout.bands) stage.label([band.label], [band.x, band.y, band.z], 'band')
    if (layout.peers.length === 0) {
      stage.label(['No peers or routed models', layout.authorized ? 'federation has no enrolled peers' : 'operator access shows enrolled peers'], [0, -3, 0], 'empty')
    }
  }

  /** Models as atoms coloured by class, solid once loaded, one orbit per capability. */
  chips(from, chips, group, near) {
    const { stage } = this
    for (const chip of chips) {
      const at = [chip.x, chip.y, chip.z]
      const look = LOOK[chip.residency] ?? LOOK.available
      stage.bond([from.x, from.y, from.z], at, 'baseline', 0.035)
      const atom = stage.sphere(`class-${chip.class}`, 0.22, at, { opacity: chip.ready ? look.opacity : 0.3 })
      stage.pick(atom, group)
      if (look.pulse) this.pulsing.push(atom)
      if (look.ring) stage.orbit(look.ring, 0.28, at, 0)
      chip.capabilities.forEach((capability, k) => stage.orbit(`cap-${capability}`, 0.34 + k * 0.08, at, k * 0.7))
      stage.label([chip.name, chip.facts, chip.note].filter(Boolean), [chip.x, chip.y - 0.42, chip.z], `chip class-${chip.class}`, [0.5, 0], { near, group })
    }
  }

  /** Back the camera off to fit the layout when its size changes. */
  frame(layout) {
    const radius = layout.radius + CHIP_BOND + (layout.sphere ? 2.5 : 0)
    if (this.radius && Math.abs(radius - this.radius) / this.radius < 0.2) return
    this.radius = radius
    const { camera, controls } = this.stage
    const distance = radius / Math.sin((camera.fov * Math.PI) / 360) + 2
    controls.target.set(0, 0, 0)
    camera.position.set(0, distance * 0.62, distance * 0.78)
  }
}

const ROLE_FOR_CELL = { prompt: 'series-1', generated: 'series-2', empty: 'baseline' }

export class PlaygroundView {
  constructor(stage) {
    this.stage = stage
    this.gen = { phase: 'idle', n_ctx: 0, prompt_tokens: 0, emitted: 0 }
    this.dt = []
    stage.controls.target.set(0, 1, 0)
    stage.onTheme = () => this.build()
  }

  scene(data) {
    this.data = data
    if (this.gen.phase === 'idle') this.build()
  }

  /** `_gen` arrives as partial patches; keep a merged copy. */
  patchGen(patch) {
    const fresh = patch.id !== undefined
    if (fresh) {
      this.gen = { n_ctx: this.gen.n_ctx, kv_bytes_per_token: this.gen.kv_bytes_per_token, prompt_tokens: 0, emitted: 0 }
      this.dt = []
    }
    const before = this.gen.emitted ?? 0
    Object.assign(this.gen, patch)
    if (patch.dt_ms !== undefined && (this.gen.emitted ?? 0) > before) this.dt[this.gen.emitted - 1] = patch.dt_ms
    if (fresh || patch.n_ctx !== undefined || patch.prompt_tokens !== undefined || !this.mesh) this.build()
    else this.paint()
  }

  nCtx() {
    if (this.gen.n_ctx) return this.gen.n_ctx
    const status = this.data?.status
    return status?.models?.find((m) => m.name === status.default_model)?.n_ctx || 512
  }

  build() {
    const { stage } = this
    stage.clear()
    this.ring = contextRing(this.nCtx())
    const geometry = new THREE.BoxGeometry(0.16, 0.1, 0.16)
    this.mesh = new THREE.InstancedMesh(geometry, new THREE.MeshStandardMaterial({ roughness: 0.6 }), this.ring.cells)
    this.mesh.instanceMatrix.setUsage(THREE.DynamicDrawUsage)
    stage.content.add(this.mesh)
    const kv = reservedKv(this.gen)
    stage.label(['context window', `${this.nCtx()} positions${this.ring.perCell > 1 ? ` · ${this.ring.perCell} per cell` : ''}${kv ? ` · ${kv}` : ''}`], [0, this.ring.rows * 0.14 + 0.6, 0], 'device')
    stage.label(['prompt'], [-5.2, 0, 0], 'legend prompt', [1, 0.5])
    stage.label(['generated'], [5.2, 0, 0], 'legend generated', [0, 0.5])
    this.paint()
  }

  paint() {
    const { mesh, ring } = this
    const matrix = new THREE.Matrix4()
    const median = this.dt.length ? [...this.dt].sort((a, b) => a - b)[this.dt.length >> 1] || 1 : 1
    for (let i = 0; i < ring.cells; i++) {
      const state = cellState(i, ring.perCell, this.gen.prompt_tokens ?? 0, this.gen.emitted ?? 0)
      const p = cellPosition(i, ring.perRow)
      // Generated cells grow with per-token latency relative to the median.
      const dt = state === 'generated' && ring.perCell === 1 ? this.dt[i - (this.gen.prompt_tokens ?? 0)] : undefined
      const height = dt ? Math.min(4, Math.max(0.5, dt / median)) : 1
      matrix.compose(new THREE.Vector3(p.x, p.y + 0.05 * height, p.z), new THREE.Quaternion().setFromAxisAngle(new THREE.Vector3(0, 1, 0), -p.angle), new THREE.Vector3(1, height, 1))
      mesh.setMatrixAt(i, matrix)
      mesh.setColorAt(i, this.stage.colors[ROLE_FOR_CELL[state]])
    }
    mesh.instanceMatrix.needsUpdate = true
    if (mesh.instanceColor) mesh.instanceColor.needsUpdate = true
  }
}
