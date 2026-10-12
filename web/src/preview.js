// Feeds a stub fleet to the page's scene bridge, as Datastar does with `_scene`.
import { stubStatus } from './stub.js'

const params = new URLSearchParams(location.search)
const options = {
  peers: Number(params.get('peers') ?? 100),
  seed: Number(params.get('seed') ?? 1),
  federated: params.get('federated') === '1',
}
const form = document.getElementById('stub')
form.peers.value = options.peers
form.seed.value = options.seed
form.federated.checked = options.federated
form.addEventListener('submit', (event) => {
  event.preventDefault()
  location.search = new URLSearchParams({ peers: form.peers.value, seed: form.seed.value, federated: form.federated.checked ? '1' : '0' })
})

window.mdmaScene.patch({ _scene: JSON.stringify(stubStatus(options)) })
