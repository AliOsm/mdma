// Datastar signal patches (`_scene`, `_gen`) drive the stage.
import { wireMarkers } from './markers.js'
import { Stage } from './stage.js'
import { FederationView, MemoryView, PlaygroundView } from './views.js'

wireMarkers()

const VIEWS = { memory: MemoryView, federation: FederationView, playground: PlaygroundView }

const host = document.getElementById('scene')
const View = host && VIEWS[host.dataset.view]
if (View) {
  const view = new View(new Stage(host))
  window.mdmaScene = {
    patch(patch) {
      if (typeof patch._scene === 'string' && patch._scene) {
        try {
          view.scene(JSON.parse(patch._scene))
        } catch (error) {
          console.error('mdma-web: bad scene snapshot', error)
        }
      }
      if (patch._gen && view.patchGen) view.patchGen(patch._gen)
    },
  }
}
