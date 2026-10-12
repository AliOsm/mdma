// WebGL stage: renderer, orbit camera, HTML labels, colours from style.css.
import * as THREE from 'three'
import { OrbitControls } from 'three/addons/controls/OrbitControls.js'
import { CSS2DObject, CSS2DRenderer } from 'three/addons/renderers/CSS2DRenderer.js'

const ROLES = [
  'surface', 'grid', 'baseline', 'muted', 'series-1', 'series-2', 'series-3', 'good', 'warning', 'serious', 'critical',
  'class-generative', 'class-decision', 'class-embedding', 'class-reranker', 'cap-reasoning', 'cap-tools', 'cap-vision', 'cap-audio',
  'os-freebsd', 'os-linux', 'os-macos',
]

export class Stage {
  constructor(host) {
    this.host = host
    this.reducedMotion = matchMedia('(prefers-reduced-motion: reduce)')
    this.renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true })
    this.renderer.setPixelRatio(Math.min(devicePixelRatio, 2))
    this.labels = new CSS2DRenderer()
    this.labels.domElement.className = 'labels'
    host.append(this.renderer.domElement, this.labels.domElement)

    this.scene = new THREE.Scene()
    this.camera = new THREE.PerspectiveCamera(45, 1, 0.1, 500)
    this.camera.position.set(9, 8, 13)
    this.controls = new OrbitControls(this.camera, this.labels.domElement)
    this.controls.enableDamping = true
    this.controls.target.set(0, 2.5, 0)
    this.scene.add(new THREE.HemisphereLight(0xffffff, 0x444444, 2.2))
    const sun = new THREE.DirectionalLight(0xffffff, 1.6)
    sun.position.set(6, 12, 8)
    this.scene.add(sun)
    this.content = new THREE.Group()
    this.scene.add(this.content)
    // Materials and geometries shared across rebuilds.
    this.shared = new Map()
    // Labels that only show near the camera, and atoms that reveal them on hover.
    this.lod = []
    this.pickable = []
    this.hovered = null
    this.pointer = null
    this.raycaster = new THREE.Raycaster()
    this.labels.domElement.addEventListener('pointermove', (event) => {
      const rect = this.labels.domElement.getBoundingClientRect()
      this.pointer = new THREE.Vector2(((event.clientX - rect.left) / rect.width) * 2 - 1, -((event.clientY - rect.top) / rect.height) * 2 + 1)
    })
    this.labels.domElement.addEventListener('pointerleave', () => { this.pointer = null })

    this.readTheme()
    matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
      this.readTheme()
      this.onTheme?.()
    })
    new ResizeObserver(() => this.resize()).observe(host)
    this.resize()
    this.renderer.setAnimationLoop((time) => {
      this.onFrame?.(time, !this.reducedMotion.matches)
      this.controls.update()
      this.updateLod()
      this.renderer.render(this.scene, this.camera)
      this.labels.render(this.scene, this.camera)
    })
  }

  readTheme() {
    this.disposeShared()
    const style = getComputedStyle(this.host)
    this.colors = Object.fromEntries(ROLES.map((role) => [role, new THREE.Color(style.getPropertyValue(`--${role}`).trim() || '#888888')]))
  }

  resize() {
    const { clientWidth: w, clientHeight: h } = this.host
    if (w === 0 || h === 0) return
    this.renderer.setSize(w, h)
    this.labels.setSize(w, h)
    this.camera.aspect = w / h
    this.camera.updateProjectionMatrix()
  }

  /** Drops everything the current view built, freeing GPU buffers it owns. */
  clear() {
    const shared = new Set(this.shared.values())
    this.content.traverse((object) => {
      if (!shared.has(object.geometry)) object.geometry?.dispose()
      for (const material of [object.material].flat()) if (!shared.has(material)) material?.dispose()
    })
    this.content.clear()
    this.lod = []
    this.pickable = []
  }

  disposeShared() {
    for (const resource of this.shared.values()) resource.dispose()
    this.shared.clear()
  }

  cached(key, make) {
    if (!this.shared.has(key)) this.shared.set(key, make())
    return this.shared.get(key)
  }

  material(role, { opacity = 1, wireframe = false } = {}) {
    return this.cached(`material:${role}:${opacity}:${wireframe}`, () => {
      const color = this.colors[role] ?? this.colors.muted
      return wireframe
        ? new THREE.LineBasicMaterial({ color, transparent: opacity < 1, opacity })
        : new THREE.MeshStandardMaterial({ color, roughness: 0.55, metalness: 0.05, transparent: opacity < 1, opacity })
    })
  }

  /** Labels tagged `near` hide past that camera distance unless their group is hovered. */
  updateLod() {
    if (this.pointer && this.pickable.length) {
      this.raycaster.setFromCamera(this.pointer, this.camera)
      this.hovered = this.raycaster.intersectObjects(this.pickable, false)[0]?.object.userData.group ?? null
    } else {
      this.hovered = null
    }
    const position = new THREE.Vector3()
    for (const { label, near, group } of this.lod) {
      label.getWorldPosition(position)
      const far = group !== this.hovered && position.distanceTo(this.camera.position) > near
      if (label.userData.far !== far) {
        label.userData.far = far
        label.element.classList.toggle('far', far)
      }
    }
  }

  box(role, [w, h, d], [x, y, z], options) {
    const mesh = new THREE.Mesh(new THREE.BoxGeometry(w, h, d), this.material(role, options))
    mesh.position.set(x, y + h / 2, z)
    this.content.add(mesh)
    return mesh
  }

  outline([w, h, d], [x, y, z], role = 'baseline') {
    const edges = new THREE.LineSegments(new THREE.EdgesGeometry(new THREE.BoxGeometry(w, h, d)), this.material(role, { wireframe: true, opacity: 0.9 }))
    edges.position.set(x, y + h / 2, z)
    this.content.add(edges)
    return edges
  }

  /** Rings around a box at `count - 1` evenly spaced heights, e.g. layer boundaries. */
  bands(count, [w, h, d], [x, y, z], role = 'surface') {
    const points = []
    const [hw, hd] = [w / 2 + 0.002, d / 2 + 0.002]
    for (let i = 1; i < count; i++) {
      const by = y + (h * i) / count
      const corners = [[-hw, -hd], [hw, -hd], [hw, hd], [-hw, hd]].map(([cx, cz]) => new THREE.Vector3(x + cx, by, z + cz))
      corners.forEach((corner, j) => points.push(corner, corners[(j + 1) % 4]))
    }
    const lines = new THREE.LineSegments(new THREE.BufferGeometry().setFromPoints(points), this.material(role, { wireframe: true, opacity: 0.55 }))
    this.content.add(lines)
    return lines
  }

  sphere(role, radius, [x, y, z], options) {
    const geometry = this.cached(`sphere:${radius}`, () => new THREE.SphereGeometry(radius, 32, 16))
    const mesh = new THREE.Mesh(geometry, this.material(role, options))
    mesh.position.set(x, y, z)
    this.content.add(mesh)
    return mesh
  }

  /** A stick between two atoms. */
  bond(from, to, role = 'baseline', radius = 0.06) {
    const [a, b] = [new THREE.Vector3(...from), new THREE.Vector3(...to)]
    const geometry = this.cached(`bond:${radius}`, () => new THREE.CylinderGeometry(radius, radius, 1, 12))
    const mesh = new THREE.Mesh(geometry, this.material(role))
    mesh.scale.set(1, a.distanceTo(b), 1)
    mesh.position.copy(a).add(b).multiplyScalar(0.5)
    mesh.quaternion.setFromUnitVectors(new THREE.Vector3(0, 1, 0), b.sub(a).normalize())
    this.content.add(mesh)
    return mesh
  }

  /** A thin orbit around an atom, tilted by `tilt` radians. */
  orbit(role, radius, [x, y, z], tilt) {
    const geometry = this.cached(`orbit:${radius}`, () => new THREE.TorusGeometry(radius, 0.022, 8, 40))
    const mesh = new THREE.Mesh(geometry, this.material(role))
    mesh.position.set(x, y, z)
    mesh.rotation.set(Math.PI / 2 + tilt, tilt * 1.7, 0)
    this.content.add(mesh)
    return mesh
  }

  /** Hovering `mesh` reveals the `near` labels of `group`. */
  pick(mesh, group) {
    mesh.userData.group = group
    this.pickable.push(mesh)
  }

  line(from, to, role = 'baseline') {
    const geometry = new THREE.BufferGeometry().setFromPoints([new THREE.Vector3(...from), new THREE.Vector3(...to)])
    const line = new THREE.Line(geometry, this.material(role, { wireframe: true }))
    this.content.add(line)
    return line
  }

  /** `anchor` is the label point placed at the position ([0.5, 0.5] centers it). */
  label(lines, [x, y, z], className = '', anchor = [0.5, 0.5], { near, group } = {}) {
    const element = document.createElement('div')
    element.className = `label ${className}`.trim()
    for (const [i, text] of [lines].flat().entries()) {
      const span = document.createElement(i === 0 ? 'strong' : 'span')
      span.textContent = text
      element.append(span)
    }
    const label = new CSS2DObject(element)
    label.center.set(...anchor)
    label.position.set(x, y, z)
    this.content.add(label)
    if (near !== undefined) this.lod.push({ label, near, group })
    return label
  }
}
