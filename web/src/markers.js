// Chat marker chips: wrap the prompt's selection in a turn's markers.

/** `value` with `[start, end)` wrapped in `open` … `close`, and the caret after the wrapped text. */
export function wrap(value, start, end, open, close) {
  const inner = value.slice(start, end)
  return { value: value.slice(0, start) + open + inner + close + value.slice(end), caret: start + open.length + inner.length }
}

/** Chips carry their markers in `data-marker-open` / `data-marker-close`. */
export function wireMarkers(doc = document) {
  doc.addEventListener('click', (event) => {
    const chip = event.target.closest?.('[data-marker-open]')
    const prompt = chip && doc.getElementById('prompt')
    if (!prompt) return
    const { value, caret } = wrap(prompt.value, prompt.selectionStart, prompt.selectionEnd, chip.dataset.markerOpen, chip.dataset.markerClose ?? '')
    prompt.value = value
    prompt.focus()
    prompt.setSelectionRange(caret, caret)
    // Datastar's data-bind follows input events.
    prompt.dispatchEvent(new Event('input', { bubbles: true }))
  })
}
